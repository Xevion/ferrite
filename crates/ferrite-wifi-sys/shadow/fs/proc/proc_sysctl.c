// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces fs/proc/proc_sysctl.c. A sysctl table is only ever read and
 * written by userspace through /proc/sys, so registering one publishes
 * nothing. The proc_do* handlers stay stubs that name themselves.
 */

#include <linux/sysctl.h>

void __register_sysctl_init(const char *path, const struct ctl_table *table, const char *table_name, size_t table_size)
{
}
