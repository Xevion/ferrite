/* SPDX-License-Identifier: GPL-2.0 */
/*
 * The shim's task calls (ferrite-lkpi `task`), which the shadow scheduler and
 * fork use. Each kernel thread is one shim task, named by its task_struct.
 */
#ifndef _LX_TASK_H
#define _LX_TASK_H

#include <linux/compiler.h>

struct task_struct;

/* Queue a task running fn(arg) as task; 0, or -1 without a stack. */
extern int lx_task_create(struct task_struct *task, int (*fn)(void *), void *arg,
			  const char *name, int prio);
/* Give up the CPU; with block, stay parked until lx_task_unblock. */
extern void lx_task_schedule(int block);
extern void lx_task_unblock(struct task_struct *task);
extern struct task_struct *lx_task_current(void);
extern void lx_task_priority(struct task_struct *task, int prio);
/* End the running task; its frames are abandoned. */
extern void __noreturn lx_task_exit(void);

#endif
