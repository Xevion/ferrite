// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces kernel/irq/spurious.c. Interrupts are polled, so most polls find
 * nothing pending by design: unhandled ones are counted by the polled flow
 * handler (drivers/pci/setup-irq.c), and no line is ever disabled for them.
 */

#include <linux/interrupt.h>
#include <linux/irq.h>

bool noirqdebug __read_mostly = true;

void note_interrupt(struct irq_desc *desc, irqreturn_t action_ret)
{
}
