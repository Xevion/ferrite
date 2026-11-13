// SPDX-License-Identifier: GPL-2.0
/* Replaces fs/kernfs/dir.c: with no sysfs published, no kernfs node is ever handed out. */

#include <linux/kernfs.h>

void kernfs_get(struct kernfs_node *kn)
{
}

void kernfs_put(struct kernfs_node *kn)
{
}
