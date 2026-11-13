// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces kernel/dma/swiotlb.c. No bounce buffer pool exists, so a new
 * device has nothing to attach and the zeroed swiotlb fields of struct device
 * already say so.
 */

#include <linux/device.h>
#include <linux/swiotlb.h>

void swiotlb_dev_init(struct device *dev)
{
}
