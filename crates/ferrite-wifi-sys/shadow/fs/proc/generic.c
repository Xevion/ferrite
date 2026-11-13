// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces fs/proc/generic.c. /proc is only ever read by userspace, so an
 * entry publishes nothing: every create returns one shared placeholder whose
 * layout nothing outside fs/proc sees, and removal does nothing.
 */

#include <linux/proc_fs.h>
#include <linux/proc_ns.h>

struct proc_dir_entry {
	int unused;
};

static struct proc_dir_entry lx_proc_entry;

struct proc_dir_entry *lx_proc_placeholder(void)
{
	return &lx_proc_entry;
}

struct proc_dir_entry *proc_create_seq_private(const char *name, umode_t mode,
		struct proc_dir_entry *parent, const struct seq_operations *ops,
		unsigned int state_size, void *data)
{
	return &lx_proc_entry;
}
EXPORT_SYMBOL(proc_create_seq_private);

struct proc_dir_entry *proc_create_single_data(const char *name, umode_t mode,
		struct proc_dir_entry *parent, int (*show)(struct seq_file *, void *), void *data)
{
	return &lx_proc_entry;
}
EXPORT_SYMBOL(proc_create_single_data);

void remove_proc_entry(const char *name, struct proc_dir_entry *parent)
{
}
EXPORT_SYMBOL(remove_proc_entry);

/* Namespace inode numbers, unique as the kernel's but never freed for reuse. */
int proc_alloc_inum(unsigned int *inum)
{
	static unsigned int next = 0xf0000000U;

	*inum = next++;
	return 0;
}

void proc_free_inum(unsigned int inum)
{
}
