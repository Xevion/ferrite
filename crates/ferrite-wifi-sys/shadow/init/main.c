// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces init/main.c: the start-up that runs before the initcalls, from the
 * start_kernel and kernel_init parts Genode's lx_emul start.c keeps.
 */

#include <linux/hrtimer.h>
#include <linux/init.h>
#include <linux/interrupt.h>
#include <linux/maple_tree.h>
#include <linux/pid.h>
#include <linux/radix-tree.h>
#include <linux/sched/clock.h>
#include <linux/sched/task.h>
#include <linux/tick.h>
#include <linux/timekeeping.h>
#include <linux/timer.h>
#include <linux/wait_bit.h>
#include <linux/workqueue.h>

#include <asm/irq_regs.h>
#include <lx/task.h>
#include <lx/time.h>

/* definitions in drivers/base/base.h */
extern int devices_init(void);
extern int buses_init(void);
extern int classes_init(void);
extern int platform_bus_init(void);
extern int faux_bus_init(void);

/* Off, as without the initcall_debug parameter. */
bool initcall_debug;

enum system_states system_state __read_mostly;
EXPORT_SYMBOL(system_state);

/* The timer task: runs the clockevent handler and timer softirq each time the deadline passes. */
static int lx_timer_loop(void *arg)
{
	for (;;) {
		lx_time_handle();
		lx_task_schedule(1);
	}
	return 0;
}

static void lx_start_timer_task(void)
{
	int pid = kernel_thread(lx_timer_loop, NULL, "timer", CLONE_FS | CLONE_FILES);
	struct task_struct *task = find_task_by_pid_ns(pid, NULL);

	BUG_ON(!task);
	lx_task_priority(task, 0);
	lx_time_set_timer_task(task);
}

/* Run on the init thread, once the scheduler and kthreadd are up. */
void lx_kernel_init(void)
{
	/* the registers tick and IRQ code inspect; never a user context */
	static struct pt_regs regs;

	set_irq_regs(&regs);
	/* as rest_init leaves it: kthreadd is up, the initcalls are next */
	system_state = SYSTEM_SCHEDULING;
	wait_bit_init();
	radix_tree_init();
	maple_tree_init();

	workqueue_init_early();

	/* start_kernel's time bring-up, with the timer task up before a deadline can be armed */
	lx_start_timer_task();
	tick_init();
	timers_init();
	hrtimers_init();
	softirq_init();
	timekeeping_init();
	time_init();
	sched_clock_init();

	workqueue_init();

	/* driver_init() of drivers/base/init.c */
	devices_init();
	buses_init();
	classes_init();
	platform_bus_init();
	faux_bus_init();
}
