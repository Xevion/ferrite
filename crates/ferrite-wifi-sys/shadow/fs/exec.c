// SPDX-License-Identifier: GPL-2.0-only
/*
 * Replaces fs/exec.c: there are no programs to execute, only task names.
 */

#include <linux/sched.h>
#include <linux/string.h>

void __set_task_comm(struct task_struct *tsk, const char *buf, bool exec)
{
	strscpy_pad(tsk->comm, buf, sizeof(tsk->comm));
}
