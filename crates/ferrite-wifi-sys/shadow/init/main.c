// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces init/main.c: the start-up that runs before the initcalls, from the
 * start_kernel and kernel_init parts Genode's lx_emul start.c keeps, and the
 * userspace-style calls the programs make once the drivers are up.
 */

#include <linux/hrtimer.h>
#include <linux/init.h>
#include <linux/interrupt.h>
#include <linux/maple_tree.h>
#include <linux/netdevice.h>
#include <linux/rtnetlink.h>
#include <linux/pid.h>
#include <linux/radix-tree.h>
#include <linux/sched/clock.h>
#include <linux/sched/task.h>
#include <linux/skbuff.h>
#include <linux/tick.h>
#include <linux/timekeeping.h>
#include <linux/timer.h>
#include <linux/wait_bit.h>
#include <linux/workqueue.h>

#include <asm/irq_regs.h>
#include <lx/pci.h>
#include <lx/task.h>
#include <lx/time.h>
#include <net/net_namespace.h>

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

/*
 * The IRQ task: each time the platform reports interrupts pending, runs every
 * polled IRQ's handlers in hard IRQ context, then the softirqs irq_exit runs.
 */
static int lx_irq_loop(void *arg)
{
	for (;;) {
		lx_task_schedule(1);
		local_irq_disable();
		irq_enter();
		lx_pci_poll_irqs();
		irq_exit();
		local_irq_enable();
	}
	return 0;
}

static void lx_start_irq_task(void)
{
	int pid = kernel_thread(lx_irq_loop, NULL, "irq", CLONE_FS | CLONE_FILES);
	struct task_struct *task = find_task_by_pid_ns(pid, NULL);

	BUG_ON(!task);
	lx_task_priority(task, 0);
	lx_irq_set_task(task);
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
	early_irq_init();

	workqueue_init_early();

	/* start_kernel's time bring-up, with the timer task up before a deadline can be armed */
	lx_start_timer_task();
	tick_init();
	timers_init();
	hrtimers_init();
	softirq_init();
	lx_start_irq_task();
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

	/* start_kernel sets up init_net, and with it the device list every netdev joins */
	net_ns_init();

	/* sock_init's skb caches; net/socket.c is not built */
	skb_init();
}

/*
 * Find the first wireless interface (op 0) or bring it up (1) or down (2), as
 * `ip link set` does, copying its name to name. -ENODEV when none is
 * registered, else what dev_open returned (0 for the others).
 */
int lx_wireless_ctl(int op, char name[IFNAMSIZ])
{
	struct net_device *dev, *found = NULL;
	int err = 0;

	rtnl_lock();
	for_each_netdev(&init_net, dev) {
		if (dev->ieee80211_ptr) {
			found = dev;
			break;
		}
	}
	if (!found) {
		rtnl_unlock();
		return -ENODEV;
	}
	strscpy(name, found->name, IFNAMSIZ);
	if (op == 1)
		err = dev_open(found, NULL);
	else if (op == 2)
		dev_close(found);
	rtnl_unlock();
	return err;
}
