// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces fs/libfs.c. There is no VFS: a pinned pseudo filesystem is a
 * zeroed vfsmount and an anonymous inode a zeroed inode, enough for the few
 * callers that only hold them. Adapted from Genode's lx_emul shadow
 * (Stefan Kalkowski, Genode Labs).
 */

#include <linux/fs.h>
#include <linux/mount.h>
#include <linux/slab.h>

int simple_pin_fs(struct file_system_type *type, struct vfsmount **mount, int *count)
{
	if (!mount)
		return -EFAULT;

	if (!*mount)
		*mount = kzalloc(sizeof(struct vfsmount), GFP_KERNEL);

	if (!*mount)
		return -ENOMEM;

	if (count)
		++*count;

	return 0;
}

void simple_release_fs(struct vfsmount **mount, int *count)
{
	if (count && --*count > 0)
		return;

	kfree(*mount);
	*mount = NULL;
}

struct inode *alloc_anon_inode(struct super_block *sb)
{
	struct inode *inode = kzalloc(sizeof(struct inode), GFP_KERNEL);

	return inode ? inode : ERR_PTR(-ENOMEM);
}
