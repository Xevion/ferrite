// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces drivers/pci/pci.c and the parts of drivers/pci/devres.c the
 * drivers call. Enabling a device and bus mastering go to the platform
 * (ferrite-lkpi `pci`); power management, wake-up and resets are absent, and
 * regions are never contended. After Genode's lx_emul shadow pci.c.
 */

#include <linux/device.h>
#include <linux/pci.h>

#include <lx/pci.h>

int pci_enable_device(struct pci_dev *dev)
{
	int err;

	if (atomic_inc_return(&dev->enable_cnt) > 1)
		return 0;
	err = lx_pci_enable(lx_pci_index(dev));
	if (err) {
		atomic_dec(&dev->enable_cnt);
		return err;
	}
	pci_info(dev, "enabled\n");
	return 0;
}
EXPORT_SYMBOL(pci_enable_device);

/* Memory-only enabling is the same decode switch here. */
int pci_enable_device_mem(struct pci_dev *dev)
{
	return pci_enable_device(dev);
}
EXPORT_SYMBOL(pci_enable_device_mem);

/* No power state is ever entered, so there is no config space to save or restore. */
int pci_save_state(struct pci_dev *dev)
{
	return 0;
}
EXPORT_SYMBOL(pci_save_state);

void pci_restore_state(struct pci_dev *dev)
{
}
EXPORT_SYMBOL(pci_restore_state);

static void lx_set_master(struct pci_dev *dev, bool on)
{
	if (!lx_pci_set_master(lx_pci_index(dev), on))
		dev->is_busmaster = on;
}

void pci_set_master(struct pci_dev *dev)
{
	lx_set_master(dev, true);
}
EXPORT_SYMBOL(pci_set_master);

void pci_clear_master(struct pci_dev *dev)
{
	lx_set_master(dev, false);
}
EXPORT_SYMBOL(pci_clear_master);

/* Decode stays on, as the firmware left it; the last disable stops the device's DMA. */
void pci_disable_device(struct pci_dev *dev)
{
	if (atomic_dec_return(&dev->enable_cnt) != 0)
		return;
	lx_set_master(dev, false);
	pci_info(dev, "disabled\n");
}
EXPORT_SYMBOL(pci_disable_device);

static void lx_pcim_disable(void *pdev)
{
	pci_disable_device(pdev);
}

int pcim_enable_device(struct pci_dev *pdev)
{
	int err = devm_add_action(&pdev->dev, lx_pcim_disable, pdev);

	if (err)
		return err;
	err = pci_enable_device(pdev);
	if (err)
		devm_remove_action(&pdev->dev, lx_pcim_disable, pdev);
	else
		pdev->is_managed = true;
	return err;
}
EXPORT_SYMBOL(pcim_enable_device);

u8 pci_find_capability(struct pci_dev *dev, int cap)
{
	u16 status;
	u8 pos, id;
	int ttl = 48;

	pci_read_config_word(dev, PCI_STATUS, &status);
	if (!(status & PCI_STATUS_CAP_LIST))
		return 0;
	pci_read_config_byte(dev, PCI_CAPABILITY_LIST, &pos);
	while (pos >= 0x40 && ttl--) {
		pos &= ~3;
		pci_read_config_byte(dev, pos + PCI_CAP_LIST_ID, &id);
		if (id == 0xff)
			return 0;
		if (id == cap)
			return pos;
		pci_read_config_byte(dev, pos + PCI_CAP_LIST_NEXT, &pos);
	}
	return 0;
}
EXPORT_SYMBOL(pci_find_capability);

u16 pci_find_ext_capability(struct pci_dev *dev, int cap)
{
	int ttl = (PCI_CFG_SPACE_EXP_SIZE - PCI_CFG_SPACE_SIZE) / 8;
	int pos = PCI_CFG_SPACE_SIZE;
	u32 header;

	if (dev->cfg_size <= PCI_CFG_SPACE_SIZE)
		return 0;
	if (pci_read_config_dword(dev, pos, &header) || header == 0 || header == ~0u)
		return 0;
	while (ttl-- > 0) {
		if (PCI_EXT_CAP_ID(header) == cap && pos != 0)
			return pos;
		pos = PCI_EXT_CAP_NEXT(header);
		if (pos < PCI_CFG_SPACE_SIZE)
			break;
		if (pci_read_config_dword(dev, pos, &header))
			break;
	}
	return 0;
}
EXPORT_SYMBOL_GPL(pci_find_ext_capability);

bool pci_pme_capable(struct pci_dev *dev, pci_power_t state)
{
	return false;
}
EXPORT_SYMBOL(pci_pme_capable);

int pci_enable_wake(struct pci_dev *dev, pci_power_t state, bool enable)
{
	return 0;
}
EXPORT_SYMBOL(pci_enable_wake);

/* ACS isolates functions for an IOMMU's groups; nothing here relies on it. */
void pci_enable_acs(struct pci_dev *dev)
{
}

/* No reset method is offered; drivers treat -ENOTTY as "not supported". */
int pci_reset_function(struct pci_dev *dev)
{
	pci_warn(dev, "function reset requested; not supported\n");
	return -ENOTTY;
}
EXPORT_SYMBOL_GPL(pci_reset_function);

int pci_request_regions(struct pci_dev *pdev, const char *res_name)
{
	return 0;
}
EXPORT_SYMBOL(pci_request_regions);

void pci_release_regions(struct pci_dev *pdev)
{
}
EXPORT_SYMBOL(pci_release_regions);

int pcim_request_all_regions(struct pci_dev *pdev, const char *name)
{
	return 0;
}
EXPORT_SYMBOL(pcim_request_all_regions);
