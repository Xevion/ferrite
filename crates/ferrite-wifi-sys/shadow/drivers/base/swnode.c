// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces drivers/base/swnode.c. No software node is ever registered, so a
 * device never has one to attach or detach and no fwnode is one.
 */

#include <linux/device.h>
#include <linux/property.h>
#include "base.h"

void software_node_notify(struct device *dev)
{
}

void software_node_notify_remove(struct device *dev)
{
}

bool is_software_node(const struct fwnode_handle *fwnode)
{
	return false;
}
