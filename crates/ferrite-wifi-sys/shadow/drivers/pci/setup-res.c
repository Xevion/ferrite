// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces drivers/pci/setup-res.c. The firmware assigns every BAR before an
 * app runs and nothing here moves one, so a BAR it left unassigned stays so.
 */

#include <linux/pci.h>

int pci_assign_resource(struct pci_dev *dev, int resno)
{
	pci_err(dev, "BAR %d is unassigned and cannot be assigned here\n", resno);
	return -ENODEV;
}
EXPORT_SYMBOL(pci_assign_resource);
