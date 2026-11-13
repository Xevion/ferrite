// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces arch/x86/lib/delay.c. A delay spins on the shim's clock
 * (ferrite-lkpi `time`) for at least as long as asked; it never schedules,
 * as a delay may run in atomic context.
 */

#include <linux/delay.h>
#include <linux/math64.h>

#include <lx/time.h>

static void lx_delay_ns(u64 ns)
{
	u64 end = lx_time_counter() + ns;

	while (lx_time_counter() < end)
		cpu_relax();
}

/* xloops is the delay in 2^-32 seconds: udelay passes usecs * 0x10c7, ndelay nsecs * 5. */
void __const_udelay(unsigned long xloops)
{
	lx_delay_ns(mul_u64_u32_shr(xloops, NSEC_PER_SEC, 32) + 1);
}
EXPORT_SYMBOL(__const_udelay);

void __udelay(unsigned long usecs)
{
	lx_delay_ns((u64)usecs * NSEC_PER_USEC);
}
EXPORT_SYMBOL(__udelay);

void __ndelay(unsigned long nsecs)
{
	lx_delay_ns(nsecs);
}
EXPORT_SYMBOL(__ndelay);
