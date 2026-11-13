// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces fs/proc/proc_net.c: /proc/net publishes nothing, as fs/proc/generic.c.
 */

#include <linux/proc_fs.h>

extern struct proc_dir_entry *lx_proc_placeholder(void);

struct proc_dir_entry *proc_create_net_data(const char *name, umode_t mode,
		struct proc_dir_entry *parent, const struct seq_operations *ops,
		unsigned int state_size, void *data)
{
	return lx_proc_placeholder();
}
EXPORT_SYMBOL_GPL(proc_create_net_data);
