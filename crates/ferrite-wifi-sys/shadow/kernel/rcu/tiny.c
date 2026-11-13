// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces kernel/rcu/tiny.c. With one CPU and no preemption, every entry to
 * schedule() is a grace period: the other tasks are parked in schedule(), and
 * the caller holds no read-side section, since rcu_read_lock() raises the
 * preempt count that schedule() checks. So callbacks queue here and run from
 * rcu_qs(), which schedule() calls.
 */

#include <linux/rcupdate.h>

static struct rcu_head *pending;
static struct rcu_head **pending_tail = &pending;

void call_rcu(struct rcu_head *head, rcu_callback_t func)
{
	head->func = func;
	head->next = NULL;
	*pending_tail = head;
	pending_tail = &head->next;
}
EXPORT_SYMBOL_GPL(call_rcu);

/* A callback may queue more; they run in the same pass. */
void rcu_qs(void)
{
	while (pending) {
		struct rcu_head *head = pending;

		pending = head->next;
		if (!pending)
			pending_tail = &pending;
		head->func(head);
	}
}

/* Grace periods come from schedule(), not the tick. */
void rcu_sched_clock_irq(int user)
{
}
