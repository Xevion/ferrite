// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces arch/x86/kernel/time.c. Clocksources here are never mapped into a
 * vDSO, so registering one has no clock mode to translate.
 */

#include <linux/clocksource.h>

void clocksource_arch_init(struct clocksource *cs)
{
}
