// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces mm/slab_common.c and stands in for mm/slub.c: a small SLUB over
 * the page allocator.
 *
 * A slab is a compound block whose struct slab (overlaying its struct page)
 * holds the cache, a freelist threaded through the free objects and the
 * in-use count. kmalloc has power-of-two classes from KMALLOC_MIN_SIZE to
 * PAGE_SIZE / 2 plus 96 and 192, each object aligned to its power-of-two
 * size; larger requests are compound pages typed large_kmalloc. kfree tells
 * the two apart from the page, as SLUB does. A cache's constructor runs when
 * a slab is made, so a freed object stays constructed. Caches are not merged,
 * an empty slab goes back unless it is the cache's last, and slabs of
 * SLAB_TYPESAFE_BY_RCU caches go back only when the cache is destroyed.
 */

#include <linux/mm.h>
#include <linux/rcupdate.h>
#include <linux/slab.h>
#include <linux/string.h>

#include "internal.h"
#include "slab.h"

extern void lx_mem_fault(const char *what, unsigned long addr) __attribute__((__noreturn__));

#define LX_KMALLOC_MAX (PAGE_SIZE / 2)

struct lx_cache {
	struct kmem_cache s;
	/* slabs with at least one free object */
	struct list_head partial;
	unsigned int order;
	unsigned int objects;
	unsigned long slabs;
};

static struct lx_cache *lx_cache(struct kmem_cache *s)
{
	return container_of(s, struct lx_cache, s);
}

#define KMALLOC_CLASS(index, bytes)						\
	[index] = {								\
		.s = { .name = "kmalloc-" #bytes, .size = bytes,		\
		       .object_size = bytes, .align = ARCH_KMALLOC_MINALIGN,	\
		       .flags = SLAB_KMALLOC | SLAB_NO_MERGE, .refcount = 1 },	\
		.partial = LIST_HEAD_INIT(kmalloc_classes[index].partial),	\
		.objects = PAGE_SIZE / bytes,					\
	}

static struct lx_cache kmalloc_classes[KMALLOC_SHIFT_HIGH + 1] = {
	KMALLOC_CLASS(1, 96),
	KMALLOC_CLASS(2, 192),
	KMALLOC_CLASS(3, 8),
	KMALLOC_CLASS(4, 16),
	KMALLOC_CLASS(5, 32),
	KMALLOC_CLASS(6, 64),
	KMALLOC_CLASS(7, 128),
	KMALLOC_CLASS(8, 256),
	KMALLOC_CLASS(9, 512),
	KMALLOC_CLASS(10, 1024),
	KMALLOC_CLASS(11, 2048),
};

#define KMALLOC_BUCKETS {							\
	[1] = &kmalloc_classes[1].s, [2] = &kmalloc_classes[2].s,		\
	[3] = &kmalloc_classes[3].s, [4] = &kmalloc_classes[4].s,		\
	[5] = &kmalloc_classes[5].s, [6] = &kmalloc_classes[6].s,		\
	[7] = &kmalloc_classes[7].s, [8] = &kmalloc_classes[8].s,		\
	[9] = &kmalloc_classes[9].s, [10] = &kmalloc_classes[10].s,		\
	[11] = &kmalloc_classes[11].s,						\
}

/* Classes above PAGE_SIZE / 2 stay NULL: __kmalloc_cache_noprof sends those to pages. */
kmem_buckets kmalloc_caches[NR_KMALLOC_TYPES] __ro_after_init = {
	[0 ... NR_KMALLOC_TYPES - 1] = KMALLOC_BUCKETS,
};
EXPORT_SYMBOL(kmalloc_caches);

static void **freepointer(struct kmem_cache *s, void *object)
{
	return object + s->offset;
}

static bool new_slab(struct lx_cache *c, gfp_t flags)
{
	struct kmem_cache *s = &c->s;
	struct page *page;
	struct slab *slab;
	void *start;
	unsigned int i;

	flags &= ~__GFP_ZERO;
	page = __alloc_frozen_pages_noprof(flags | __GFP_COMP, c->order, NUMA_NO_NODE, NULL);
	if (!page)
		return false;
	__folio_set_slab(page_folio(page));
	slab = page_slab(page);
	slab->slab_cache = s;
	slab->objects = c->objects;
	slab->inuse = 0;
	slab->frozen = 0;
	start = page_address(page);
	slab->freelist = start;
	for (i = 0; i < c->objects; i++) {
		void *object = start + i * s->size;

		if (s->ctor)
			s->ctor(object);
		*freepointer(s, object) = i + 1 < c->objects ? object + s->size : NULL;
	}
	list_add(&slab->slab_list, &c->partial);
	c->slabs++;
	return true;
}

static void discard_slab(struct lx_cache *c, struct slab *slab)
{
	struct folio *folio = slab_folio(slab);

	list_del(&slab->slab_list);
	__folio_clear_slab(folio);
	c->slabs--;
	free_frozen_pages(&folio->page, c->order);
}

static void *slab_alloc(struct kmem_cache *s, gfp_t flags)
{
	struct lx_cache *c = lx_cache(s);
	struct slab *slab;
	void *object;

	if (list_empty(&c->partial) && !new_slab(c, flags))
		return NULL;
	slab = list_first_entry(&c->partial, struct slab, slab_list);
	object = slab->freelist;
	slab->freelist = *freepointer(s, object);
	slab->inuse++;
	if (!slab->freelist)
		list_del(&slab->slab_list);
	if (slab_want_init_on_alloc(flags, s))
		memset(object, 0, s->object_size);
	return object;
}

static void slab_free(struct slab *slab, void *object)
{
	struct kmem_cache *s = slab->slab_cache;
	struct lx_cache *c = lx_cache(s);
	unsigned long offset = object - slab_address(slab);

	if (offset % s->size || offset / s->size >= slab->objects)
		lx_mem_fault("free of a pointer that is no slab object", (unsigned long)object);
	if (slab->freelist == object)
		lx_mem_fault("double free of slab object", (unsigned long)object);
	if (!slab->freelist)
		list_add(&slab->slab_list, &c->partial);
	*freepointer(s, object) = slab->freelist;
	slab->freelist = object;
	slab->inuse--;
	if (!slab->inuse && !(s->flags & SLAB_TYPESAFE_BY_RCU) && !list_is_singular(&c->partial))
		discard_slab(c, slab);
}

static void free_large_kmalloc(struct folio *folio, const void *object)
{
	if (!folio_test_large_kmalloc(folio) || object != folio_address(folio))
		lx_mem_fault("kfree of a pointer kmalloc did not return", (unsigned long)object);
	__folio_clear_large_kmalloc(folio);
	free_frozen_pages(&folio->page, folio_order(folio));
}

void kfree(const void *object)
{
	struct folio *folio;

	if (unlikely(ZERO_OR_NULL_PTR(object)))
		return;
	folio = virt_to_folio(object);
	if (unlikely(!folio_test_slab(folio))) {
		free_large_kmalloc(folio, object);
		return;
	}
	slab_free(folio_slab(folio), (void *)object);
}
EXPORT_SYMBOL(kfree);

void *__kmalloc_large_noprof(size_t size, gfp_t flags)
{
	unsigned int order = get_order(size);
	struct page *page;

	if (order > MAX_PAGE_ORDER) {
		WARN_ON_ONCE(!(flags & __GFP_NOWARN));
		return NULL;
	}
	page = __alloc_frozen_pages_noprof(flags | __GFP_COMP, order, NUMA_NO_NODE, NULL);
	if (!page)
		return NULL;
	__folio_set_large_kmalloc(page_folio(page));
	return page_address(page);
}
EXPORT_SYMBOL(__kmalloc_large_noprof);

void *__kmalloc_noprof(DECL_BUCKET_PARAMS(size, b), gfp_t flags)
{
	if (unlikely(size > LX_KMALLOC_MAX))
		return __kmalloc_large_noprof(size, flags);
	if (unlikely(!size))
		return ZERO_SIZE_PTR;
	return slab_alloc(kmalloc_caches[KMALLOC_NORMAL][__kmalloc_index(size, false)], flags);
}
EXPORT_SYMBOL(__kmalloc_noprof);

void *__kmalloc_node_noprof(DECL_BUCKET_PARAMS(size, b), gfp_t flags, int node)
{
	return __kmalloc_noprof(PASS_BUCKET_PARAMS(size, b), flags);
}
EXPORT_SYMBOL(__kmalloc_node_noprof);

void *__kmalloc_node_track_caller_noprof(DECL_BUCKET_PARAMS(size, b), gfp_t flags, int node,
					 unsigned long caller)
{
	return __kmalloc_noprof(PASS_BUCKET_PARAMS(size, b), flags);
}
EXPORT_SYMBOL(__kmalloc_node_track_caller_noprof);

void *__kmalloc_cache_noprof(struct kmem_cache *s, gfp_t flags, size_t size)
{
	return s ? slab_alloc(s, flags) : __kmalloc_noprof(PASS_BUCKET_PARAMS(size, NULL), flags);
}
EXPORT_SYMBOL(__kmalloc_cache_noprof);

void *__kmalloc_cache_node_noprof(struct kmem_cache *s, gfp_t flags, int node, size_t size)
{
	return __kmalloc_cache_noprof(s, flags, size);
}
EXPORT_SYMBOL(__kmalloc_cache_node_noprof);

size_t ksize(const void *object)
{
	struct folio *folio;

	if (unlikely(ZERO_OR_NULL_PTR(object)))
		return 0;
	folio = virt_to_folio(object);
	if (folio_test_slab(folio))
		return folio_slab(folio)->slab_cache->object_size;
	return folio_size(folio);
}
EXPORT_SYMBOL(ksize);

size_t kmalloc_size_roundup(size_t size)
{
	if (!size)
		return 0;
	if (size <= LX_KMALLOC_MAX)
		return kmalloc_caches[KMALLOC_NORMAL][__kmalloc_index(size, false)]->object_size;
	if (size > KMALLOC_MAX_SIZE)
		return size;
	return PAGE_SIZE << get_order(size);
}
EXPORT_SYMBOL(kmalloc_size_roundup);

void kfree_sensitive(const void *object)
{
	size_t size = ksize(object);

	if (size)
		memzero_explicit((void *)object, size);
	kfree(object);
}
EXPORT_SYMBOL(kfree_sensitive);

/* As SLUB's __do_krealloc: keep the object when it is big and aligned enough. */
void *krealloc_node_align_noprof(const void *p, size_t new_size, unsigned long align, gfp_t flags, int nid)
{
	size_t old_size = ksize(p);
	void *ret;

	if (unlikely(!new_size)) {
		kfree(p);
		return ZERO_SIZE_PTR;
	}
	if (old_size && new_size <= old_size && IS_ALIGNED((unsigned long)p, align)) {
		if (want_init_on_alloc(flags))
			memset((void *)p + new_size, 0, old_size - new_size);
		return (void *)p;
	}
	ret = __kmalloc_noprof(PASS_BUCKET_PARAMS(new_size, NULL), flags);
	if (ret && old_size) {
		memcpy(ret, p, min(new_size, old_size));
		kfree(p);
	}
	return ret;
}
EXPORT_SYMBOL(krealloc_node_align_noprof);

/* Blocks above PAGE_SIZE / 2 are pages already, so kvmalloc never needs a vmalloc fallback. */
void *__kvmalloc_node_noprof(DECL_BUCKET_PARAMS(size, b), unsigned long align, gfp_t flags, int node)
{
	return __kmalloc_noprof(PASS_BUCKET_PARAMS(size, b), flags);
}
EXPORT_SYMBOL(__kvmalloc_node_noprof);

void *kvrealloc_node_align_noprof(const void *p, size_t size, unsigned long align, gfp_t flags, int nid)
{
	return krealloc_node_align_noprof(p, size, align, flags, nid);
}
EXPORT_SYMBOL(kvrealloc_node_align_noprof);

void kvfree(const void *addr)
{
	kfree(addr);
}
EXPORT_SYMBOL(kvfree);

/* SLUB's calculate_alignment, with L1_CACHE_BYTES for cache_line_size(), which reads boot_cpu_data. */
static unsigned int cache_alignment(slab_flags_t flags, unsigned int align, unsigned int size)
{
	if (flags & SLAB_HWCACHE_ALIGN) {
		unsigned int ralign = L1_CACHE_BYTES;

		while (size <= ralign / 2)
			ralign /= 2;
		align = max(align, ralign);
	}
	align = max(align, (unsigned int)ARCH_SLAB_MINALIGN);
	return ALIGN(align, sizeof(void *));
}

struct kmem_cache *__kmem_cache_create_args(const char *name, unsigned int object_size,
					    struct kmem_cache_args *args, slab_flags_t flags)
{
	static struct kmem_cache_args no_args;
	struct lx_cache *c = __kmalloc_noprof(PASS_BUCKET_PARAMS(sizeof(*c), NULL), GFP_KERNEL | __GFP_ZERO);
	struct kmem_cache *s;
	unsigned int size;

	if (!c)
		return NULL;
	if (!args)
		args = &no_args;
	s = &c->s;
	s->name = name;
	s->object_size = object_size;
	s->flags = flags;
	s->ctor = args->ctor;
	s->align = cache_alignment(flags, args->align, object_size);
	s->refcount = 1;
	size = ALIGN(object_size, sizeof(void *));
	if (args->use_freeptr_offset) {
		s->offset = args->freeptr_offset;
	} else if (s->ctor || (flags & SLAB_TYPESAFE_BY_RCU)) {
		/* the free pointer must not overwrite a constructed or RCU-visible object */
		s->offset = size;
		size += sizeof(void *);
	}
	s->size = ALIGN(size, s->align);
	s->inuse = object_size;
	c->order = get_order(s->size);
	while (c->order < 3 && (PAGE_SIZE << c->order) / s->size < 8)
		c->order++;
	c->objects = (PAGE_SIZE << c->order) / s->size;
	INIT_LIST_HEAD(&c->partial);
	return s;
}
EXPORT_SYMBOL(__kmem_cache_create_args);

void kmem_cache_destroy(struct kmem_cache *s)
{
	struct lx_cache *c;
	struct slab *slab, *next;

	if (!s || --s->refcount > 0)
		return;
	c = lx_cache(s);
	if (s->flags & SLAB_TYPESAFE_BY_RCU)
		rcu_barrier();
	list_for_each_entry_safe(slab, next, &c->partial, slab_list) {
		if (!slab->inuse)
			discard_slab(c, slab);
	}
	if (c->slabs) {
		pr_err("kmem_cache_destroy %s: %lu slabs still have objects, leaking the cache\n", s->name, c->slabs);
		return;
	}
	kfree(c);
}
EXPORT_SYMBOL(kmem_cache_destroy);

unsigned int kmem_cache_size(struct kmem_cache *s)
{
	return s->object_size;
}
EXPORT_SYMBOL(kmem_cache_size);

void *kmem_cache_alloc_noprof(struct kmem_cache *s, gfp_t flags)
{
	return slab_alloc(s, flags);
}
EXPORT_SYMBOL(kmem_cache_alloc_noprof);

void *kmem_cache_alloc_lru_noprof(struct kmem_cache *s, struct list_lru *lru, gfp_t flags)
{
	return slab_alloc(s, flags);
}
EXPORT_SYMBOL(kmem_cache_alloc_lru_noprof);

void *kmem_cache_alloc_node_noprof(struct kmem_cache *s, gfp_t flags, int node)
{
	return slab_alloc(s, flags);
}
EXPORT_SYMBOL(kmem_cache_alloc_node_noprof);

void kmem_cache_free(struct kmem_cache *s, void *object)
{
	struct slab *slab;

	if (unlikely(ZERO_OR_NULL_PTR(object)))
		return;
	slab = virt_to_slab(object);
	if (!slab || slab->slab_cache != s)
		lx_mem_fault("kmem_cache_free of an object from another cache", (unsigned long)object);
	slab_free(slab, object);
}
EXPORT_SYMBOL(kmem_cache_free);

int kmem_cache_alloc_bulk_noprof(struct kmem_cache *s, gfp_t flags, size_t size, void **p)
{
	size_t i;

	for (i = 0; i < size; i++) {
		p[i] = slab_alloc(s, flags);
		if (!p[i]) {
			kmem_cache_free_bulk(s, i, p);
			return 0;
		}
	}
	return size;
}
EXPORT_SYMBOL(kmem_cache_alloc_bulk_noprof);

/* A NULL cache means each object's own, as in SLUB. */
void kmem_cache_free_bulk(struct kmem_cache *s, size_t size, void **p)
{
	size_t i;

	for (i = 0; i < size; i++) {
		if (s)
			kmem_cache_free(s, p[i]);
		else
			kfree(p[i]);
	}
}
EXPORT_SYMBOL(kmem_cache_free_bulk);

/*
 * Sheaves, which the maple tree preallocates its nodes in: here a plain
 * array of objects taken from the cache up front, so a later allocation
 * under GFP_NOWAIT cannot fail.
 */
struct slab_sheaf {
	unsigned int capacity;
	unsigned int size;
	void *objects[];
};

static struct slab_sheaf *sheaf_alloc(unsigned int capacity, gfp_t gfp)
{
	struct slab_sheaf *sheaf = kmalloc(struct_size(sheaf, objects, capacity), gfp & ~__GFP_ZERO);

	if (sheaf) {
		sheaf->capacity = capacity;
		sheaf->size = 0;
	}
	return sheaf;
}

static int sheaf_fill(struct kmem_cache *s, gfp_t gfp, struct slab_sheaf *sheaf, unsigned int size)
{
	while (sheaf->size < size) {
		void *object = slab_alloc(s, gfp & ~__GFP_ZERO);

		if (!object)
			return -ENOMEM;
		sheaf->objects[sheaf->size++] = object;
	}
	return 0;
}

struct slab_sheaf *kmem_cache_prefill_sheaf(struct kmem_cache *s, gfp_t gfp, unsigned int size)
{
	struct slab_sheaf *sheaf = sheaf_alloc(size, gfp);

	if (sheaf && sheaf_fill(s, gfp, sheaf, size)) {
		kmem_cache_return_sheaf(s, gfp, sheaf);
		return NULL;
	}
	return sheaf;
}
EXPORT_SYMBOL(kmem_cache_prefill_sheaf);

int kmem_cache_refill_sheaf(struct kmem_cache *s, gfp_t gfp, struct slab_sheaf **sheafp, unsigned int size)
{
	struct slab_sheaf *sheaf = *sheafp;

	if (!sheaf) {
		*sheafp = kmem_cache_prefill_sheaf(s, gfp, size);
		return *sheafp ? 0 : -ENOMEM;
	}
	if (sheaf->capacity < size) {
		struct slab_sheaf *bigger = sheaf_alloc(size, gfp);

		if (!bigger)
			return -ENOMEM;
		memcpy(bigger->objects, sheaf->objects, sheaf->size * sizeof(void *));
		bigger->size = sheaf->size;
		kfree(sheaf);
		*sheafp = sheaf = bigger;
	}
	return sheaf_fill(s, gfp, sheaf, size);
}
EXPORT_SYMBOL(kmem_cache_refill_sheaf);

void kmem_cache_return_sheaf(struct kmem_cache *s, gfp_t gfp, struct slab_sheaf *sheaf)
{
	kmem_cache_free_bulk(s, sheaf->size, sheaf->objects);
	kfree(sheaf);
}
EXPORT_SYMBOL(kmem_cache_return_sheaf);

void *kmem_cache_alloc_from_sheaf_noprof(struct kmem_cache *s, gfp_t gfp, struct slab_sheaf *sheaf)
{
	void *object;

	if (!sheaf->size)
		return NULL;
	object = sheaf->objects[--sheaf->size];
	if (gfp & __GFP_ZERO)
		memset(object, 0, s->object_size);
	return object;
}
EXPORT_SYMBOL(kmem_cache_alloc_from_sheaf_noprof);

unsigned int kmem_cache_sheaf_size(struct slab_sheaf *sheaf)
{
	return sheaf->size;
}
EXPORT_SYMBOL(kmem_cache_sheaf_size);

/* NULL asks for the default kmalloc buckets, the only ones there are. */
kmem_buckets *kmem_buckets_create(const char *name, slab_flags_t flags, unsigned int useroffset,
				  unsigned int usersize, void (*ctor)(void *))
{
	return NULL;
}
EXPORT_SYMBOL(kmem_buckets_create);
