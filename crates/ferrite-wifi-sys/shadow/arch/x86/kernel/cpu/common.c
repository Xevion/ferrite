// SPDX-License-Identifier: GPL-2.0-only
/*
 * Replaces arch/x86/kernel/cpu/common.c: the per-CPU words the C reads on every
 * path. Without SMP they are plain globals. The shim writes current_task on
 * each task switch; the main stack runs as init_task.
 */

#include <linux/sched/task.h>
#include <linux/percpu.h>
#include <linux/preempt.h>
#include <asm/current.h>
#include <asm/preempt.h>

DEFINE_PER_CPU_CACHE_HOT(struct task_struct *, current_task) = &init_task;
EXPORT_PER_CPU_SYMBOL(current_task);

/* PREEMPT_ENABLED: a zero count, with the inverted need-resched bit clear. */
DEFINE_PER_CPU_CACHE_HOT(int, __preempt_count) = PREEMPT_ENABLED;
EXPORT_PER_CPU_SYMBOL(__preempt_count);
