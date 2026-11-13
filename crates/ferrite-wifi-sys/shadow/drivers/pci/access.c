// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces drivers/pci/access.c. Config space belongs to the platform
 * (ferrite-lkpi `pci`); every access is one call there, which the firmware
 * serialises, so no config lock is taken.
 */

#include <linux/pci.h>

#include <lx/pci.h>

static int lx_read(const struct pci_dev *dev, int where, unsigned int bytes, u32 *val)
{
	if (where < 0 || where + bytes > dev->cfg_size || (where & (bytes - 1))) {
		*val = ~0u;
		return PCIBIOS_BAD_REGISTER_NUMBER;
	}
	return lx_pci_config_read(lx_pci_index(dev), where, bytes, val) ? PCIBIOS_DEVICE_NOT_FOUND
									   : PCIBIOS_SUCCESSFUL;
}

static int lx_write(const struct pci_dev *dev, int where, unsigned int bytes, u32 val)
{
	if (where < 0 || where + bytes > dev->cfg_size || (where & (bytes - 1)))
		return PCIBIOS_BAD_REGISTER_NUMBER;
	return lx_pci_config_write(lx_pci_index(dev), where, bytes, val) ? PCIBIOS_DEVICE_NOT_FOUND
									    : PCIBIOS_SUCCESSFUL;
}

int pci_read_config_byte(const struct pci_dev *dev, int where, u8 *val)
{
	u32 v;
	int ret = lx_read(dev, where, 1, &v);

	*val = v;
	return ret;
}
EXPORT_SYMBOL(pci_read_config_byte);

int pci_read_config_word(const struct pci_dev *dev, int where, u16 *val)
{
	u32 v;
	int ret = lx_read(dev, where, 2, &v);

	*val = v;
	return ret;
}
EXPORT_SYMBOL(pci_read_config_word);

int pci_read_config_dword(const struct pci_dev *dev, int where, u32 *val)
{
	return lx_read(dev, where, 4, val);
}
EXPORT_SYMBOL(pci_read_config_dword);

int pci_write_config_byte(const struct pci_dev *dev, int where, u8 val)
{
	return lx_write(dev, where, 1, val);
}
EXPORT_SYMBOL(pci_write_config_byte);

int pci_write_config_word(const struct pci_dev *dev, int where, u16 val)
{
	return lx_write(dev, where, 2, val);
}
EXPORT_SYMBOL(pci_write_config_word);

int pci_write_config_dword(const struct pci_dev *dev, int where, u32 val)
{
	return lx_write(dev, where, 4, val);
}
EXPORT_SYMBOL(pci_write_config_dword);

/* Every register of a version 2 capability exists; a function without one reads zero, as Linux does. */
int pcie_capability_read_word(struct pci_dev *dev, int pos, u16 *val)
{
	*val = 0;
	if (pos & 1)
		return PCIBIOS_BAD_REGISTER_NUMBER;
	if (!pci_is_pcie(dev))
		return PCIBIOS_SUCCESSFUL;
	return pci_read_config_word(dev, pci_pcie_cap(dev) + pos, val);
}
EXPORT_SYMBOL(pcie_capability_read_word);

int pcie_capability_read_dword(struct pci_dev *dev, int pos, u32 *val)
{
	*val = 0;
	if (pos & 3)
		return PCIBIOS_BAD_REGISTER_NUMBER;
	if (!pci_is_pcie(dev))
		return PCIBIOS_SUCCESSFUL;
	return pci_read_config_dword(dev, pci_pcie_cap(dev) + pos, val);
}
EXPORT_SYMBOL(pcie_capability_read_dword);

int pcie_capability_write_word(struct pci_dev *dev, int pos, u16 val)
{
	if (pos & 1)
		return PCIBIOS_BAD_REGISTER_NUMBER;
	if (!pci_is_pcie(dev))
		return PCIBIOS_SUCCESSFUL;
	return pci_write_config_word(dev, pci_pcie_cap(dev) + pos, val);
}
EXPORT_SYMBOL(pcie_capability_write_word);

int pcie_capability_clear_and_set_word_unlocked(struct pci_dev *dev, int pos, u16 clear, u16 set)
{
	u16 val;
	int ret = pcie_capability_read_word(dev, pos, &val);

	if (ret)
		return ret;
	return pcie_capability_write_word(dev, pos, (val & ~clear) | set);
}
EXPORT_SYMBOL(pcie_capability_clear_and_set_word_unlocked);

int pcie_capability_clear_and_set_word_locked(struct pci_dev *dev, int pos, u16 clear, u16 set)
{
	return pcie_capability_clear_and_set_word_unlocked(dev, pos, clear, set);
}
EXPORT_SYMBOL(pcie_capability_clear_and_set_word_locked);
