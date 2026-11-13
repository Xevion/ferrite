// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces drivers/pci/setup-irq.c and arch/x86/pci's pcibios IRQ hooks.
 * There is no interrupt line: each device gets one legacy IRQ number on a
 * chip that masks nothing, and the IRQ task (init/main.c) runs every such
 * IRQ's handlers when the platform's poll cadence wakes it. The driver sees
 * INTx, since MSI is configured out. After Genode's lx_emul setup-irq.c.
 */

#include <linux/interrupt.h>
#include <linux/irq.h>
#include <linux/kernel_stat.h>
#include <linux/pci.h>

#include <lx/pci.h>
#include "../../kernel/irq/internals.h"

#define LX_MAX_POLLED 8

static unsigned int lx_polled[LX_MAX_POLLED];
static unsigned int lx_polled_count;
unsigned long lx_irq_polls, lx_irq_unhandled;

static void lx_irq_noop(struct irq_data *data)
{
}

static struct irq_chip lx_polled_chip = {
	.name = "polled",
	.irq_mask = lx_irq_noop,
	.irq_unmask = lx_irq_noop,
};

/* handle_simple_irq, keeping the handlers' verdict for the spurious count. */
static void lx_polled_flow(struct irq_desc *desc)
{
	irqreturn_t ret;

	raw_spin_lock(&desc->lock);
	if (!desc->action || irqd_irq_disabled(&desc->irq_data) || irqd_irq_inprogress(&desc->irq_data)) {
		raw_spin_unlock(&desc->lock);
		return;
	}
	kstat_incr_irqs_this_cpu(desc);
	ret = handle_irq_event(desc);
	raw_spin_unlock(&desc->lock);
	lx_irq_polls++;
	if (ret == IRQ_NONE)
		lx_irq_unhandled++;
}

void pci_assign_irq(struct pci_dev *dev)
{
	int irq;

	if (dev->irq)
		return;
	if (lx_polled_count == LX_MAX_POLLED) {
		pci_err(dev, "no polled IRQ left\n");
		return;
	}
	irq = irq_alloc_desc_from(NR_IRQS_LEGACY, NUMA_NO_NODE);
	if (irq < 0) {
		pci_err(dev, "allocating an IRQ descriptor failed: %d\n", irq);
		return;
	}
	irq_set_chip_and_handler(irq, &lx_polled_chip, lx_polled_flow);
	lx_polled[lx_polled_count++] = irq;
	dev->irq = irq;
	pci_info(dev, "polled IRQ %d\n", irq);
}

int pcibios_alloc_irq(struct pci_dev *dev)
{
	return 0;
}

void pcibios_free_irq(struct pci_dev *dev)
{
}

void lx_pci_poll_irqs(void)
{
	unsigned int i;

	for (i = 0; i < lx_polled_count; i++)
		generic_handle_irq(lx_polled[i]);
}
