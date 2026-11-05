# Host tests for the portable RWK core (main/rwk). Needs gcc and OpenSSL 3 headers.
#   make -C tests/host test
RWK := ../../main/rwk
CFLAGS ?= -std=c11 -O1 -g -Wall -Wextra -Werror -fsanitize=address,undefined
SRCS := test_rwk.c rwk_crypto_openssl.c $(RWK)/rwk_blake2s.c $(RWK)/rwk_wg.c \
        $(RWK)/rwk_frame.c $(RWK)/rwk_inner.c $(RWK)/rwk_stun.c $(RWK)/rwk_jitter.c \
        $(RWK)/rwk_edge.c

test_rwk: $(SRCS) $(wildcard $(RWK)/*.h)
	$(CC) $(CFLAGS) -o $@ $(SRCS) -lcrypto

test: test_rwk
	./test_rwk

clean:
	rm -f test_rwk
.PHONY: test clean
