// SPDX-License-Identifier: GPL-2.0
/* Replaces fs/sysfs/symlink.c: links are not published, see shadow/fs/sysfs/dir.c. */

#include <linux/kobject.h>
#include <linux/sysfs.h>

int sysfs_create_link(struct kobject *kobj, struct kobject *target, const char *name)
{
	return 0;
}

void sysfs_remove_link(struct kobject *kobj, const char *name)
{
}

void sysfs_delete_link(struct kobject *kobj, struct kobject *targ, const char *name)
{
}

int sysfs_rename_link_ns(struct kobject *kobj, struct kobject *targ, const char *old, const char *new, const void *new_ns)
{
	return 0;
}

int sysfs_link_change_owner(struct kobject *kobj, struct kobject *targ, const char *name, kuid_t kuid, kgid_t kgid)
{
	return 0;
}
