// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces arch/x86/kernel/time.c: the platform clocksource, a oneshot
 * clockevent and sched_clock over the shim's nanosecond counter, and the boot
 * wall clock. Adapted from Genode's lx_emul clocksource.c and time_initial.c
 * (Stefan Kalkowski, Pirmin Duss). Clocksources here are never mapped into a
 * vDSO, so registering one has no clock mode to translate.
 */

#include <linux/clockchips.h>
#include <linux/clocksource.h>
#include <linux/interrupt.h>
#include <linux/sched/clock.h>
#include <linux/tick.h>
#include <linux/time.h>
#include <linux/timekeeper_internal.h>

#include <lx/time.h>

#include "../../../kernel/time/tick-internal.h"

void clocksource_arch_init(struct clocksource *cs)
{
}

/* The counter ticks in nanoseconds. */
#define LX_TIME_RATE NSEC_PER_SEC

static bool lx_time_ready;

static int lx_set_next_event(unsigned long evt, struct clock_event_device *ced)
{
	lx_time_arm(evt);
	return 0;
}

static int lx_set_state_shutdown(struct clock_event_device *ced)
{
	lx_time_stop();
	return 0;
}

static u64 lx_clocksource_read(struct clocksource *cs)
{
	return lx_time_counter();
}

static struct clocksource lx_clocksource = {
	.name   = "lx_counter",
	.rating = 400,
	.read   = lx_clocksource_read,
	.mask   = CLOCKSOURCE_MASK(64),
	.flags  = CLOCK_SOURCE_IS_CONTINUOUS,
};

static struct clock_event_device lx_clockevent = {
	.name                      = "lx_timer",
	.features                  = CLOCK_EVT_FEAT_ONESHOT,
	.rating                    = 400,
	.set_state_shutdown        = lx_set_state_shutdown,
	.set_state_oneshot_stopped = lx_set_state_shutdown,
	.set_next_event            = lx_set_next_event,
};

void __init time_init(void)
{
	lx_clockevent.cpumask = cpumask_of(smp_processor_id());
	clocksource_register_hz(&lx_clocksource, LX_TIME_RATE);
	clockevents_config_and_register(&lx_clockevent, LX_TIME_RATE, 0xf, 0x7fffffff);
	lx_time_ready = true;
}

noinstr u64 sched_clock_noinstr(void)
{
	return lx_time_counter();
}

u64 sched_clock(void)
{
	return lx_time_counter();
}

void read_persistent_clock64(struct timespec64 *ts)
{
	*ts = ns_to_timespec64(lx_time_boot_wall_ns());
}

/* The clockevent fired: run its handler, then the timer softirq it raised. */
void lx_time_handle(void)
{
	if (!lx_time_ready)
		return;

	lx_clockevent.event_handler(&lx_clockevent);
	if (local_softirq_pending() & (1u << TIMER_SOFTIRQ))
		do_softirq();
}

/* On every schedule(): move jiffies and the wall time if a tick period passed. */
void lx_time_update_jiffies(void)
{
	if (!lx_time_ready || ktime_before(ktime_get(), tick_next_period))
		return;

	/* tick_nohz_idle_stop_tick refuses with a softirq pending */
	if (local_softirq_pending() & ~SOFTIRQ_HOTPLUG_SAFE_MASK)
		return;

	tick_nohz_idle_enter();
	tick_nohz_idle_stop_tick();
	tick_nohz_idle_restart_tick();
	tick_nohz_idle_exit();
}

/* There is no vDSO to publish the time to. */
void update_vsyscall(struct timekeeper *tk)
{
}

void update_vsyscall_tz(void)
{
}

/* Nothing in the archive queues irq_work, so the tick has none to run. */
void irq_work_tick(void)
{
}

bool irq_work_needs_cpu(void)
{
	return false;
}
