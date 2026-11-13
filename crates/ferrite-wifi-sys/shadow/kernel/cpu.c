// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces kernel/cpu.c: one CPU, always online, and no hotplug.
 * Adapted from Genode's lx_emul shadow (Stefan Kalkowski, Genode Labs).
 */

#include <linux/cpu.h>
#include <linux/cpuhotplug.h>
#include <linux/cpumask.h>

struct cpumask __cpu_possible_mask = { .bits[0] = 1 };
EXPORT_SYMBOL(__cpu_possible_mask);
struct cpumask __cpu_online_mask = { .bits[0] = 1 };
EXPORT_SYMBOL(__cpu_online_mask);
struct cpumask __cpu_present_mask = { .bits[0] = 1 };
EXPORT_SYMBOL(__cpu_present_mask);
struct cpumask __cpu_active_mask = { .bits[0] = 1 };
EXPORT_SYMBOL(__cpu_active_mask);

#define MASK_DECLARE_1(x) [x+1][0] = (1UL << (x))
#define MASK_DECLARE_2(x) MASK_DECLARE_1(x), MASK_DECLARE_1(x+1)
#define MASK_DECLARE_4(x) MASK_DECLARE_2(x), MASK_DECLARE_2(x+2)
#define MASK_DECLARE_8(x) MASK_DECLARE_4(x), MASK_DECLARE_4(x+4)

const unsigned long cpu_bit_bitmap[BITS_PER_LONG+1][BITS_TO_LONGS(NR_CPUS)] = {
	MASK_DECLARE_8(0),  MASK_DECLARE_8(8),
	MASK_DECLARE_8(16), MASK_DECLARE_8(24),
	MASK_DECLARE_8(32), MASK_DECLARE_8(40),
	MASK_DECLARE_8(48), MASK_DECLARE_8(56),
};
EXPORT_SYMBOL_GPL(cpu_bit_bitmap);

const DECLARE_BITMAP(cpu_all_bits, NR_CPUS) = CPU_BITS_ALL;
EXPORT_SYMBOL(cpu_all_bits);

/* No CPU ever comes or goes, so no callback is run and every setup succeeds. */
int __cpuhp_setup_state(enum cpuhp_state state, const char *name, bool invoke,
			int (*startup)(unsigned int cpu),
			int (*teardown)(unsigned int cpu), bool multi_instance)
{
	return 0;
}
EXPORT_SYMBOL(__cpuhp_setup_state);
