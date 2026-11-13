// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces fs/sysfs/dir.c. There is no userspace to see a sysfs tree, so
 * every kobject is "added" without publishing anything, which is what
 * CONFIG_SYSFS=n does through the inline stubs of linux/sysfs.h. Genode's
 * lx_emul has no sysfs either.
 */

#include <linux/kernfs.h>
#include <linux/kobject.h>
#include <linux/sysfs.h>

/* Every directory is this one inert node, since code like kernfs_enable_ns writes through kobj->sd. */
static struct kernfs_node inert_dir = { .flags = KERNFS_DIR };

int sysfs_create_dir_ns(struct kobject *kobj, const void *ns)
{
	kobj->sd = &inert_dir;
	return 0;
}

void sysfs_remove_dir(struct kobject *kobj)
{
	kobj->sd = NULL;
}

int sysfs_rename_dir_ns(struct kobject *kobj, const char *new_name, const void *new_ns)
{
	return 0;
}

int sysfs_move_dir_ns(struct kobject *kobj, struct kobject *new_parent_kobj, const void *new_ns)
{
	return 0;
}
