// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces kernel/irq/proc.c. There is no /proc, so an IRQ and its handlers
 * have no entries to create or remove.
 */

#include <linux/interrupt.h>
#include <linux/irq.h>

void register_irq_proc(unsigned int irq, struct irq_desc *desc)
{
}

void unregister_irq_proc(unsigned int irq, struct irq_desc *desc)
{
}

void register_handler_proc(unsigned int irq, struct irqaction *action)
{
}

void unregister_handler_proc(unsigned int irq, struct irqaction *action)
{
}
