// SPDX-License-Identifier: GPL-2.0
/* Replaces fs/sysfs/group.c: attribute groups are not published, see shadow/fs/sysfs/dir.c. */

#include <linux/kobject.h>
#include <linux/sysfs.h>

int sysfs_create_group(struct kobject *kobj, const struct attribute_group *grp)
{
	return 0;
}

int sysfs_create_groups(struct kobject *kobj, const struct attribute_group **groups)
{
	return 0;
}

int sysfs_add_file_to_group(struct kobject *kobj, const struct attribute *attr, const char *group)
{
	return 0;
}

void sysfs_remove_group(struct kobject *kobj, const struct attribute_group *grp)
{
}

void sysfs_remove_groups(struct kobject *kobj, const struct attribute_group **groups)
{
}

int sysfs_groups_change_owner(struct kobject *kobj, const struct attribute_group **groups, kuid_t kuid, kgid_t kgid)
{
	return 0;
}
