// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces kernel/rcu/tiny.c. With one CPU and no preemption, every entry to
 * schedule() is a grace period: the other tasks are parked in schedule(), and
 * the caller holds no read-side section, since rcu_read_lock() raises the
 * preempt count that schedule() checks. So callbacks queue here and run from
 * rcu_qs(), which schedule() calls.
 */

#include <linux/mm.h>
#include <linux/rcupdate.h>
#include <linux/rcutiny.h>

/* The largest offset of an rcu_head in an object that kvfree_rcu stores in place of a callback. */
#define KVFREE_RCU_OFFSET 4096

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

/* Counts the passes that found the CPU quiescent, for the polled grace-period cookies. */
static unsigned long gp_seq;

/* A callback may queue more; they run in the same pass. A small "function" is a kvfree_rcu offset. */
void rcu_qs(void)
{
	gp_seq++;
	while (pending) {
		struct rcu_head *head = pending;
		unsigned long offset = (unsigned long)head->func;

		pending = head->next;
		if (!pending)
			pending_tail = &pending;
		if (offset < KVFREE_RCU_OFFSET)
			kvfree((void *)head - offset);
		else
			head->func(head);
	}
}

/* Grace periods come from schedule(), not the tick. */
void rcu_sched_clock_irq(int user)
{
}

/* Every queued callback runs in one pass, so none is outstanding when this returns. */
void rcu_barrier(void)
{
	rcu_qs();
}
EXPORT_SYMBOL_GPL(rcu_barrier);

/* The caller is outside a read section and the other tasks are parked, so now is a grace period. */
void synchronize_rcu(void)
{
	might_sleep();
	rcu_qs();
}
EXPORT_SYMBOL_GPL(synchronize_rcu);

void kvfree_call_rcu(struct rcu_head *head, void *ptr)
{
	if (head) {
		call_rcu(head, (rcu_callback_t)((void *)head - ptr));
		return;
	}
	synchronize_rcu();
	kvfree(ptr);
}
EXPORT_SYMBOL_GPL(kvfree_call_rcu);

unsigned long get_state_synchronize_rcu(void)
{
	return gp_seq;
}
EXPORT_SYMBOL_GPL(get_state_synchronize_rcu);

unsigned long start_poll_synchronize_rcu(void)
{
	return gp_seq;
}
EXPORT_SYMBOL_GPL(start_poll_synchronize_rcu);

bool poll_state_synchronize_rcu(unsigned long oldstate)
{
	return oldstate != gp_seq;
}
EXPORT_SYMBOL_GPL(poll_state_synchronize_rcu);
