// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces mm/percpu.c. With one CPU a per-cpu object is a plain zeroed
 * allocation and per_cpu_ptr() is the identity. Adapted from Genode's lx_emul
 * shadow (Stefan Kalkowski, Genode Labs).
 */

#include <linux/log2.h>
#include <linux/percpu.h>
#include <linux/slab.h>

/* kmalloc aligns a power-of-two size to itself, which covers the alignment. */
void __percpu *pcpu_alloc_noprof(size_t size, size_t align, bool reserved, gfp_t gfp)
{
	return __kmalloc_noprof(roundup_pow_of_two(max(size, align)), gfp | __GFP_ZERO);
}
EXPORT_SYMBOL_GPL(pcpu_alloc_noprof);

void free_percpu(void __percpu *ptr)
{
	kfree((void __force *)ptr);
}
EXPORT_SYMBOL_GPL(free_percpu);
