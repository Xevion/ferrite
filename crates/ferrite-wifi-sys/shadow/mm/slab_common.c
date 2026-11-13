// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces mm/slab_common.c. kmalloc is the shim's (ferrite-lkpi `mem`), which
 * ignores the cache, so the table the inline kmalloc indexes stays all NULL.
 */

#include <linux/slab.h>

kmem_buckets kmalloc_caches[NR_KMALLOC_TYPES] __ro_after_init;
EXPORT_SYMBOL(kmalloc_caches);
