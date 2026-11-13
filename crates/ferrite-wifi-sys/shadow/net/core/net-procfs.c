// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces net/core/net-procfs.c. There is no /proc to publish the device,
 * softnet and ptype files in, and zero is what a proc-less build returns.
 */

#include <linux/netdevice.h>
#include "dev.h"

int __init dev_proc_init(void)
{
	return 0;
}
