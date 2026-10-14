#!/usr/bin/perl

# Tests for parsing of the zstd directives.

###############################################################################

use warnings;
use strict;

use Test::More;

BEGIN { use FindBin; chdir($FindBin::Bin); }

use lib 'lib';
use Test::Nginx;

###############################################################################

select STDERR; $| = 1;
select STDOUT; $| = 1;

my $t = Test::Nginx->new()->has(qw/http/)->plan(41);

###############################################################################

my $ok = qr/test is successful/;

# valid configurations

like(check($t, <<'EOF'), $ok, 'all directives');
zstd on;
zstd_types text/plain application/json;
zstd_buffers 16 8k;
zstd_comp_level 19;
zstd_window 8m;
zstd_min_length 256;
zstd_reuse_memory off;
zstd_retention_policy bounded 16m;
zstd_idle_encoders 16;
zstd_static always;
EOF

# the directives of tokers/zstd-nginx-module

like(check($t, 'zstd_comp_level -5;'), $ok, 'negative level');
like(check($t, 'zstd_comp_level -131072;'), $ok, 'fastest level');
like(check($t, 'zstd_comp_level 22;'), $ok, 'strongest level');
like(check($t, 'zstd_min_length 256 512;'), $ok, 'min length takes more arguments');
like(check($t, 'zstd_dict_file /nonexistent/dict;'),
	qr/"zstd_dict_file" is not supported and is ignored.*$ok/s,
	'dict file is ignored');

like(check($t, 'zstd_retention_policy aggressive;'), $ok, 'aggressive');
like(check($t, 'zstd_retention_policy current_config;'), $ok, 'current_config');
like(check($t, 'zstd_retention_policy release_all;'), $ok, 'release_all');
like(check($t, 'zstd_retention_policy bounded 0;'), $ok, 'bounded 0');

like(check($t, '', 'zstd_retention_policy release_all; zstd_reuse_memory on;'),
	$ok, 'location level');

like(check($t, 'zstd_idle_encoders 0;'), $ok, 'no idle encoders');

like(check($t, 'zstd_thread_pool off; zstd_parallel_segment_size 256k; '
		. 'zstd_parallel_tasks 8;'),
	$ok, 'parallel directives');

SKIP: {
skip 'no threads', 4 unless $t->has_module('--with-threads');

like(check($t, 'zstd_thread_pool zstd;', 'zstd_thread_pool off;',
		'thread_pool zstd threads=2;'),
	$ok, 'thread pool');

like(check($t, 'zstd_thread_pool default;'), $ok, 'default thread pool');

like(check($t, 'zstd_thread_pool nope;'),
	qr/unknown thread pool "nope"/,
	'unknown thread pool');

like(check($t, 'zstd_parallel_compressors 16;'), $ok, 'parallel compressors');

}

SKIP: {
skip 'threads', 2 if $t->has_module('--with-threads');

like(check($t, 'zstd_thread_pool zstd;'),
	qr/"zstd_thread_pool" requires NGINX built with --with-threads/,
	'thread pool without threads');

like(check($t, 'zstd_parallel_compressors 16;'),
	qr/"zstd_parallel_compressors" requires NGINX built with --with-threads/,
	'parallel compressors without threads');

}

# invalid configurations

like(check($t, 'zstd_retention_policy sometimes;'),
	qr/invalid value "sometimes" in "zstd_retention_policy" directive/,
	'unknown policy');

like(check($t, 'zstd_retention_policy bounded;'),
	qr/"bounded" in "zstd_retention_policy" directive requires a size/,
	'bounded without size');

like(check($t, 'zstd_retention_policy bounded lots;'),
	qr/invalid size "lots" in "zstd_retention_policy" directive/,
	'bounded with invalid size');

like(check($t, 'zstd_retention_policy aggressive 1m;'),
	qr/"aggressive" in "zstd_retention_policy" directive does not take a size/,
	'size for a policy without one');

like(check($t, 'zstd_retention_policy release_all; '
		. 'zstd_retention_policy aggressive;'),
	qr/"zstd_retention_policy" directive is duplicate/,
	'duplicate policy');

like(check($t, '', 'zstd_idle_encoders 2;'),
	qr/"zstd_idle_encoders" directive is not allowed here/,
	'idle encoders in a location');

like(check($t, 'zstd_idle_encoders 2000;'),
	qr/value must be between 0 and 1024/,
	'idle encoders bounds');

like(check($t, 'zstd_reuse_memory maybe;'),
	qr/invalid value "maybe" in "zstd_reuse_memory" directive/,
	'reuse memory flag');

like(check($t, 'zstd_window 3k;'),
	qr/must be 1k, 2k, 4k, 8k, 16k, 32k, 64k, 128k, 256k, 512k, 1m, 2m, 4m or 8m/,
	'window size');

like(check($t, 'zstd_window 16m;'),
	qr/must be 1k, 2k, 4k, 8k, 16k, 32k, 64k, 128k, 256k, 512k, 1m, 2m, 4m or 8m/,
	'window over 8m');

like(check($t, 'zstd_comp_level 23;'),
	qr/must be between -131072 and 22, excluding 0/,
	'comp level bounds');

like(check($t, 'zstd_comp_level -131073;'),
	qr/must be between -131072 and 22, excluding 0/,
	'comp level lower bound');

like(check($t, 'zstd_comp_level 0;'),
	qr/must be between -131072 and 22, excluding 0/,
	'comp level 0');

like(check($t, 'zstd_comp_level fast;'),
	qr/"zstd_comp_level" directive invalid number/,
	'comp level not a number');

like(check($t, 'zstd_comp_level 3; zstd_comp_level -1;'),
	qr/"zstd_comp_level" directive is duplicate/,
	'duplicate comp level');

like(check($t, '', 'zstd_dict_file /dict;'),
	qr/"zstd_dict_file" directive is not allowed here/,
	'dict file in a location');

like(check($t, 'zstd_static sometimes;'),
	qr/invalid value "sometimes"/,
	'static value');

like(check($t, 'zstd_buffers 4;'),
	qr/invalid number of arguments in "zstd_buffers" directive/,
	'buffers arguments');

like(check($t, 'zstd_thread_pool off; zstd_thread_pool off;'),
	qr/"zstd_thread_pool" directive is duplicate/,
	'duplicate thread pool');

like(check($t, 'zstd_parallel_segment_size 32k;'),
	qr/must be between 64k and 16m/,
	'segment size too small');

like(check($t, 'zstd_parallel_segment_size 32m;'),
	qr/must be between 64k and 16m/,
	'segment size too large');

like(check($t, 'zstd_parallel_tasks 0;'),
	qr/value must be between 1 and 1024/,
	'parallel tasks bounds');

###############################################################################

# Writes nginx.conf with `$http` in the http block, `$location` in a location
# and `$main` in the main context, and returns the output of `nginx -t`. Takes
# `$t` as an argument: a sub closing over it would keep it alive until global
# destruction, too late for its end-of-test checks.
sub check {
	my ($t, $http, $location, $main) = @_;
	$location //= '';
	$main //= '';

	$t->write_file_expand('nginx.conf', <<"EOF");

%%TEST_GLOBALS%%

daemon off;

$main

events {
}

http {
    %%TEST_GLOBALS_HTTP%%

    $http

    server {
        listen       127.0.0.1:8080;
        server_name  localhost;

        location / {
            $location
        }
    }
}

EOF

	my $d = $t->testdir();
	return `$Test::Nginx::NGINX -t -p $d/ -c nginx.conf -e error.log 2>&1`;
}

###############################################################################
