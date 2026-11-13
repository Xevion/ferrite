// SPDX-License-Identifier: GPL-2.0
/* Replaces fs/sysfs/mount.c: sysfs_change_owner has no node to chown, see shadow/fs/sysfs/dir.c. */

#include <linux/kobject.h>
#include <linux/sysfs.h>

int sysfs_change_owner(struct kobject *kobj, kuid_t kuid, kgid_t kgid)
{
	return 0;
}
