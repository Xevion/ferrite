// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces arch/x86/kernel/irq.c and the IRQ-number hooks of
 * arch/x86/kernel/apic/vector.c. Only the per-cpu counters the softirq and
 * interrupt code reads are defined; there is no vector table. Adapted from
 * Genode's lx_emul shadow (Stefan Kalkowski, Genode Labs).
 */

#include <linux/interrupt.h>
#include <linux/irq.h>
#include <asm/hardirq.h>

DEFINE_PER_CPU_CACHE_HOT(u16, __softirq_pending);
EXPORT_PER_CPU_SYMBOL(__softirq_pending);

/* early_irq_init preallocates the legacy range; polled device IRQs are allocated above it. */
int __init arch_probe_nr_irqs(void)
{
	return NR_IRQS_LEGACY;
}

int __init arch_early_irq_init(void)
{
	return 0;
}
