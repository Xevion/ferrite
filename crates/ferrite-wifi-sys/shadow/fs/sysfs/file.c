// SPDX-License-Identifier: GPL-2.0
/* Replaces fs/sysfs/file.c: attribute files are not published, see shadow/fs/sysfs/dir.c. */

#include <linux/kobject.h>
#include <linux/sysfs.h>

int sysfs_create_file_ns(struct kobject *kobj, const struct attribute *attr, const void *ns)
{
	return 0;
}

int sysfs_create_bin_file(struct kobject *kobj, const struct bin_attribute *attr)
{
	return 0;
}

void sysfs_remove_file_ns(struct kobject *kobj, const struct attribute *attr, const void *ns)
{
}

void sysfs_remove_bin_file(struct kobject *kobj, const struct bin_attribute *attr)
{
}

/* No file exists, so there is no reader to remove it from under. */
bool sysfs_remove_file_self(struct kobject *kobj, const struct attribute *attr)
{
	return false;
}

int sysfs_file_change_owner(struct kobject *kobj, const char *name, kuid_t kuid, kgid_t kgid)
{
	return 0;
}

/* No file exists, so no reader can be active either. */
struct kernfs_node *sysfs_break_active_protection(struct kobject *kobj, const struct attribute *attr)
{
	return NULL;
}

void sysfs_unbreak_active_protection(struct kernfs_node *kn)
{
}
