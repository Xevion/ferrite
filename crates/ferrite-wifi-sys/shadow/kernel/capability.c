// SPDX-License-Identifier: GPL-2.0-only
/*
 * Replaces kernel/capability.c. MULTIUSER is off, so every task is root and ns_capable is true in the
 * headers; a socket's opener is root too, whatever file (none, for a kernel socket) it was opened by.
 */

#include <linux/capability.h>
#include <linux/export.h>

bool file_ns_capable(const struct file *file, struct user_namespace *ns, int cap)
{
	return cap_valid(cap);
}
EXPORT_SYMBOL(file_ns_capable);
