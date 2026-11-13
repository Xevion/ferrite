// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces kernel/sched/core.c: the scheduler is the shim's cooperative one.
 * Adapted from Genode's lx_emul shadow (Stefan Kalkowski, Genode Labs).
 *
 * A task sleeps by setting its state and calling schedule(), which blocks it
 * iff the state is not TASK_RUNNING at the call, so a wake between the two is
 * not lost. A wake sets TASK_RUNNING and makes the task runnable.
 */

#include <linux/kernel_stat.h>
#include <linux/preempt.h>
#include <linux/sched.h>
#include <linux/sched/debug.h>
#include <linux/sched/stat.h>
#include <linux/sched/task.h>
#include <linux/sched/wake_q.h>
#include <linux/sched/nohz.h>
#include <linux/irqflags.h>
#include <uapi/linux/sched/types.h>

#include <lx/task.h>

#include "sched.h"
#include "../workqueue_internal.h"

DEFINE_PER_CPU_SHARED_ALIGNED(struct rq, runqueues);
DEFINE_PER_CPU(struct kernel_stat, kstat);

/* What every switch requires: no lock, preempt-off or RCU section held, interrupts on. */
static void check_schedulable(void)
{
	BUG_ON(preempt_count());
	BUG_ON(irqs_disabled());
}

static void __schedule(void)
{
	check_schedulable();
	rcu_qs();
	lx_task_schedule(current->__state != TASK_RUNNING);
}

asmlinkage __visible void __sched schedule(void)
{
	struct task_struct *tsk = current;

	if (tsk->__state && (tsk->flags & PF_WQ_WORKER))
		wq_worker_sleeping(tsk);

	__schedule();

	if (tsk->flags & PF_WQ_WORKER)
		wq_worker_running(tsk);
}
EXPORT_SYMBOL(schedule);

void __sched schedule_preempt_disabled(void)
{
	sched_preempt_enable_no_resched();
	schedule();
	preempt_disable();
}

void __sched io_schedule(void)
{
	schedule();
}
EXPORT_SYMBOL(io_schedule);

long __sched io_schedule_timeout(long timeout)
{
	return schedule_timeout(timeout);
}
EXPORT_SYMBOL(io_schedule_timeout);

int io_schedule_prepare(void)
{
	return 0;
}

void io_schedule_finish(int token)
{
}

int __cond_resched(void)
{
	if (should_resched(0)) {
		schedule();
		return 1;
	}
	return 0;
}
EXPORT_SYMBOL(__cond_resched);

int try_to_wake_up(struct task_struct *p, unsigned int state, int wake_flags)
{
	if (!(READ_ONCE(p->__state) & state))
		return 0;

	WRITE_ONCE(p->__state, TASK_RUNNING);
	if (p != current)
		lx_task_unblock(p);
	return 1;
}

int wake_up_process(struct task_struct *p)
{
	return try_to_wake_up(p, TASK_NORMAL, 0);
}
EXPORT_SYMBOL(wake_up_process);

int wake_up_state(struct task_struct *p, unsigned int state)
{
	return try_to_wake_up(p, state, 0);
}

int default_wake_function(wait_queue_entry_t *curr, unsigned mode, int wake_flags,
			  void *key)
{
	return try_to_wake_up(curr->private, mode, wake_flags);
}
EXPORT_SYMBOL(default_wake_function);

static bool __wake_q_add(struct wake_q_head *head, struct task_struct *task)
{
	struct wake_q_node *node = &task->wake_q;

	if (unlikely(cmpxchg_relaxed(&node->next, NULL, WAKE_Q_TAIL)))
		return false;

	*head->lastp = node;
	head->lastp = &node->next;
	return true;
}

void wake_q_add(struct wake_q_head *head, struct task_struct *task)
{
	if (__wake_q_add(head, task))
		get_task_struct(task);
}

void wake_q_add_safe(struct wake_q_head *head, struct task_struct *task)
{
	if (!__wake_q_add(head, task))
		put_task_struct(task);
}

void wake_up_q(struct wake_q_head *head)
{
	struct wake_q_node *node = head->first;

	while (node != WAKE_Q_TAIL) {
		struct task_struct *task = container_of(node, struct task_struct, wake_q);

		node = node->next;
		WRITE_ONCE(task->wake_q.next, NULL);
		wake_up_process(task);
		put_task_struct(task);
	}
}

void set_user_nice(struct task_struct *p, long nice)
{
	p->static_prio = NICE_TO_PRIO(nice);
	p->prio = p->static_prio;
	p->normal_prio = p->static_prio;
	lx_task_priority(p, p->static_prio);
}
EXPORT_SYMBOL(set_user_nice);

/* Policies are not modelled; every task keeps its priority. */
int sched_setscheduler_nocheck(struct task_struct *p, int policy,
			       const struct sched_param *param)
{
	return 0;
}

void sched_set_fifo(struct task_struct *p)
{
}
EXPORT_SYMBOL_GPL(sched_set_fifo);

void sched_set_fifo_low(struct task_struct *p)
{
}
EXPORT_SYMBOL_GPL(sched_set_fifo_low);

int set_cpus_allowed_ptr(struct task_struct *p, const struct cpumask *new_mask)
{
	return 0;
}
EXPORT_SYMBOL_GPL(set_cpus_allowed_ptr);

void do_set_cpus_allowed(struct task_struct *p, const struct cpumask *new_mask)
{
}

/* A task off the CPU is always parked in schedule(), since nothing preempts. */
unsigned long wait_task_inactive(struct task_struct *p, unsigned int match_state)
{
	return p == current ? 0 : 1;
}

int idle_cpu(int cpu)
{
	return 1;
}

unsigned int nr_iowait_cpu(int cpu)
{
	return 0;
}

void wake_up_nohz_cpu(int cpu)
{
}

void __might_resched(const char *file, int line, unsigned int offsets)
{
	unsigned int preempt_offset = offsets & MIGHT_RESCHED_PREEMPT_MASK;

	if (preempt_count() == preempt_offset && !irqs_disabled())
		return;
	lx_warn(file, line);
}
EXPORT_SYMBOL(__might_resched);

void __might_sleep(const char *file, int line)
{
	__might_resched(file, line, 0);
}
EXPORT_SYMBOL(__might_sleep);
