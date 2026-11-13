/* SPDX-License-Identifier: GPL-2.0 */
/*
 * The shim's time calls (ferrite-lkpi `time`), which the shadow arch time code
 * uses: one nanosecond counter and one oneshot deadline on the scheduler.
 */
#ifndef _LX_TIME_H
#define _LX_TIME_H

#include <linux/types.h>

struct task_struct;

/* Nanoseconds of the platform clock. */
extern u64 lx_time_counter(void);
/* Arm the one deadline delta_ns from now; the timer task is woken past it. */
extern void lx_time_arm(u64 delta_ns);
extern void lx_time_stop(void);
extern void lx_time_set_timer_task(struct task_struct *task);
/* Wall clock at start-up, nanoseconds since the Unix epoch. */
extern u64 lx_time_boot_wall_ns(void);

/* Defined in arch/x86/kernel/time.c. */
void lx_time_handle(void);
void lx_time_update_jiffies(void);

#endif
