// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces drivers/char/misc.c. A misc device exists to get a /dev node for
 * userspace to open, and there is none, so registering one succeeds without
 * creating anything.
 */

#include <linux/miscdevice.h>

int misc_register(struct miscdevice *misc)
{
	return 0;
}

void misc_deregister(struct miscdevice *misc)
{
}
