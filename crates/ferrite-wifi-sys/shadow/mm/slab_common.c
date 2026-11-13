// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces mm/slab_common.c. kmalloc is the shim's (ferrite-lkpi `mem`), which
 * ignores the cache, so the table the inline kmalloc indexes stays all NULL.
 * A kmem_cache is only a size, an alignment and a constructor over kmalloc;
 * adapted from Genode's lx_emul shadow of mm/slub.c.
 */

#include <linux/slab.h>
#include "slab.h"

kmem_buckets kmalloc_caches[NR_KMALLOC_TYPES] __ro_after_init;
EXPORT_SYMBOL(kmalloc_caches);

struct kmem_cache *__kmem_cache_create_args(const char *name,
					    unsigned int object_size,
					    struct kmem_cache_args *args,
					    slab_flags_t flags)
{
	struct kmem_cache *cache = __kmalloc_noprof(sizeof(*cache), GFP_KERNEL | __GFP_ZERO);

	if (!cache)
		return NULL;
	cache->name = name;
	cache->object_size = object_size;
	cache->size = object_size;
	cache->flags = flags;
	cache->align = args ? args->align : 0;
	cache->ctor = args ? args->ctor : NULL;
	cache->refcount = 1;
	return cache;
}
EXPORT_SYMBOL(__kmem_cache_create_args);

void kmem_cache_destroy(struct kmem_cache *cache)
{
	if (!cache)
		return;
	if (--cache->refcount <= 0)
		kfree(cache);
}
EXPORT_SYMBOL(kmem_cache_destroy);

unsigned int kmem_cache_size(struct kmem_cache *cache)
{
	return cache->object_size;
}
EXPORT_SYMBOL(kmem_cache_size);

/* kmalloc aligns a block to its size rounded up to a power of two, which covers the cache's alignment. */
void *kmem_cache_alloc_noprof(struct kmem_cache *cache, gfp_t flags)
{
	void *object = __kmalloc_noprof(max(cache->size, cache->align), flags);

	if (object && cache->ctor)
		cache->ctor(object);
	return object;
}
EXPORT_SYMBOL(kmem_cache_alloc_noprof);

void *kmem_cache_alloc_lru_noprof(struct kmem_cache *cache, struct list_lru *lru, gfp_t flags)
{
	return kmem_cache_alloc_noprof(cache, flags);
}
EXPORT_SYMBOL(kmem_cache_alloc_lru_noprof);

void *kmem_cache_alloc_node_noprof(struct kmem_cache *cache, gfp_t flags, int node)
{
	return kmem_cache_alloc_noprof(cache, flags);
}
EXPORT_SYMBOL(kmem_cache_alloc_node_noprof);

void kmem_cache_free(struct kmem_cache *cache, void *object)
{
	kfree(object);
}
EXPORT_SYMBOL(kmem_cache_free);

int kmem_cache_alloc_bulk_noprof(struct kmem_cache *cache, gfp_t flags, size_t size, void **p)
{
	size_t i;

	for (i = 0; i < size; i++) {
		p[i] = kmem_cache_alloc_noprof(cache, flags);
		if (!p[i]) {
			kmem_cache_free_bulk(cache, i, p);
			return 0;
		}
	}
	return size;
}
EXPORT_SYMBOL(kmem_cache_alloc_bulk_noprof);

void kmem_cache_free_bulk(struct kmem_cache *cache, size_t size, void **p)
{
	size_t i;

	for (i = 0; i < size; i++)
		kfree(p[i]);
}
EXPORT_SYMBOL(kmem_cache_free_bulk);

/* NULL asks for the default kmalloc buckets, the only ones there are. */
kmem_buckets *kmem_buckets_create(const char *name, slab_flags_t flags, unsigned int useroffset,
				  unsigned int usersize, void (*ctor)(void *))
{
	return NULL;
}
EXPORT_SYMBOL(kmem_buckets_create);
