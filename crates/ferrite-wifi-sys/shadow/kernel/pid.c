// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces kernel/pid.c: one namespace, and tasks found by walking init_task.tasks.
 * Adapted from Genode's lx_emul shadow (Stefan Kalkowski, Genode Labs).
 */

#include <linux/sched/task.h>
#include <linux/pid_namespace.h>
#include <linux/sched/task.h>

struct task_struct *find_task_by_pid_ns(pid_t nr, struct pid_namespace *ns)
{
	struct task_struct *task;

	list_for_each_entry(task, &init_task.tasks, tasks)
		if (task->pid == nr)
			return task;
	return NULL;
}

struct task_struct *find_task_by_vpid(pid_t vnr)
{
	return find_task_by_pid_ns(vnr, NULL);
}
