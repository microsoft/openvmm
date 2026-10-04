// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include <assert.h>
#include <stdio.h>
#include <string.h>

#include "amba-pl011-console.h"

struct fake_uart {
	unsigned int capacity;
	unsigned int occupied;
	unsigned int waits;
	int drain_immediately;
	unsigned char output[8192];
	unsigned int output_len;
};

static unsigned int fake_get_room(void *context, struct pl011_console_tx *tx)
{
	struct fake_uart *uart = context;
	unsigned int room;

	for (;;) {
		tx->status_reads++;
		room = pl011_console_tx_credit(uart->occupied == 0,
					      uart->occupied == uart->capacity,
					      tx->capacity);
		if (room)
			return room;
		assert(uart->occupied);
		uart->occupied--;
		uart->waits++;
	}
}

static void fake_write(void *context, unsigned char ch)
{
	struct fake_uart *uart = context;

	assert(uart->occupied < uart->capacity);
	assert(uart->output_len < sizeof(uart->output));
	uart->output[uart->output_len++] = ch;
	uart->occupied++;
	if (uart->drain_immediately)
		uart->occupied = 0;
}

static const struct pl011_console_tx_ops ops = {
	.get_room = fake_get_room,
	.write = fake_write,
};

static void emit(struct pl011_console_tx *tx, struct fake_uart *uart,
		 const unsigned char *text, unsigned int len)
{
	unsigned int i;

	for (i = 0; i < len; i++)
		pl011_console_tx_char(tx, &ops, uart, text[i]);
}

static void test_empty_fifo_read_reduction(void)
{
	struct pl011_console_tx tx = { .capacity = 32 };
	struct fake_uart uart = { .capacity = 32, .drain_immediately = 1 };
	unsigned char text[512];

	memset(text, 'A', sizeof(text));
	emit(&tx, &uart, text, sizeof(text));
	assert(tx.status_reads == 16);
	assert(tx.data_writes == 512);
	assert(uart.output_len == sizeof(text));
	assert(memcmp(uart.output, text, sizeof(text)) == 0);
}

static void test_crlf_and_credit_boundary(void)
{
	static const unsigned char text[] = "A\nB\r\n\n";
	static const unsigned char expected[] = "A\r\nB\r\r\n\r\n";
	struct pl011_console_tx tx = { .capacity = 2 };
	struct fake_uart uart = { .capacity = 2, .drain_immediately = 1 };

	emit(&tx, &uart, text, sizeof(text) - 1);
	assert(uart.output_len == sizeof(expected) - 1);
	assert(memcmp(uart.output, expected, sizeof(expected) - 1) == 0);
	assert(tx.status_reads == 5);
	assert(tx.data_writes == sizeof(expected) - 1);
}

static void test_disabled_fifo(void)
{
	static const unsigned char text[] = "A\nB";
	struct pl011_console_tx tx = { .capacity = 1 };
	struct fake_uart uart = { .capacity = 1, .drain_immediately = 1 };

	emit(&tx, &uart, text, sizeof(text) - 1);
	assert(tx.status_reads == 4);
	assert(tx.data_writes == 4);
	assert(memcmp(uart.output, "A\r\nB", 4) == 0);
}

static void test_full_and_partial_fifo(void)
{
	static const unsigned char text[] = "abcdefghijk\n";
	static const unsigned char expected[] = "abcdefghijk\r\n";
	struct pl011_console_tx tx = { .capacity = 4 };
	struct fake_uart uart = { .capacity = 4, .occupied = 4 };

	emit(&tx, &uart, text, sizeof(text) - 1);
	assert(uart.waits == sizeof(expected) - 1);
	assert(uart.occupied == uart.capacity);
	assert(uart.output_len == sizeof(expected) - 1);
	assert(memcmp(uart.output, expected, sizeof(expected) - 1) == 0);
}

static void test_credits_do_not_cross_records(void)
{
	struct pl011_console_tx first = { .capacity = 32 };
	struct pl011_console_tx second = { .capacity = 32 };
	struct fake_uart uart = { .capacity = 32, .drain_immediately = 1 };

	pl011_console_tx_char(&first, &ops, &uart, 'A');
	assert(first.room == 31);
	uart.drain_immediately = 0;
	uart.occupied = 32;
	pl011_console_tx_char(&second, &ops, &uart, 'B');
	assert(second.status_reads == 2);
	assert(uart.waits == 1);
	assert(uart.output_len == 2);
	assert(memcmp(uart.output, "AB", 2) == 0);
}

static void test_conservative_flag_combinations(void)
{
	assert(pl011_console_tx_credit(1, 1, 32) == 0);
	assert(pl011_console_tx_credit(0, 1, 32) == 0);
	assert(pl011_console_tx_credit(0, 0, 32) == 1);
	assert(pl011_console_tx_credit(1, 0, 32) == 32);
	assert(pl011_console_tx_credit(1, 0, 0) == 1);
}

int main(void)
{
	test_empty_fifo_read_reduction();
	test_crlf_and_credit_boundary();
	test_disabled_fifo();
	test_full_and_partial_fifo();
	test_credits_do_not_cross_records();
	test_conservative_flag_combinations();
	puts("PASS: 6 production FIFO-helper tests; 512 bytes use 16 status reads");
	return 0;
}
