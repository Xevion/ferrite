/* SPDX-License-Identifier: GPL-2.0 */
/*
 * The shim's firmware call (ferrite-lkpi `firmware`), which the shadow
 * firmware loader makes: a file's bytes by Linux firmware name.
 */
#ifndef _LX_FIRMWARE_H
#define _LX_FIRMWARE_H

#include <linux/types.h>

/* 0 with the bytes in *data and *size, or a negative errno (-ENOENT: no such file). */
extern int lx_firmware_load(const char *name, u8 **data, size_t *size);
/* Gives back a buffer lx_firmware_load returned. */
extern void lx_firmware_free(u8 *data, size_t size);

#endif
