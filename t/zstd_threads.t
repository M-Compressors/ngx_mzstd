#!/usr/bin/perl

# Tests for ngx_http_zstd_filter_module with zstd_thread_pool: responses
# are cut into segments and encoded on an NGINX thread pool.

###############################################################################

use warnings;
use strict;

use Test::More;

use IO::Socket::INET;

BEGIN { use FindBin; chdir($FindBin::Bin); }

use lib 'lib';
use Test::Nginx qw/ :DEFAULT http_content /;

###############################################################################

select STDERR; $| = 1;
select STDOUT; $| = 1;

my $t = Test::Nginx->new()->has(qw/http proxy ssi --with-threads/)
	->has_daemon('zstd')->plan(28)
	->write_file_expand('nginx.conf', <<'EOF');

%%TEST_GLOBALS%%

daemon off;

thread_pool zstd threads=4;

events {
}

http {
    %%TEST_GLOBALS_HTTP%%

    types {
        text/html               html;
        application/javascript  js;
    }

    log_format ratio '$uri $zstd_ratio';

    # Fewer cached compressors than responses in the tests below.
    zstd_idle_encoders 2;
    zstd_parallel_compressors 1;

    server {
        listen       127.0.0.1:8080;
        server_name  localhost;

        zstd on;
        zstd_types application/javascript;
        zstd_thread_pool zstd;
        zstd_parallel_segment_size 64k;
        zstd_parallel_tasks 2;

        access_log %%TESTDIR%%/ratio.log ratio;

        location / {
        }

        location /serial/ {
            alias %%TESTDIR%%/;
            zstd_thread_pool off;
        }

        location /default/ {
            alias %%TESTDIR%%/;
            zstd_thread_pool default;
            zstd_parallel_tasks 8;
        }

        location /noreuse/ {
            alias %%TESTDIR%%/;
            zstd_reuse_memory off;
        }

        location /release/ {
            alias %%TESTDIR%%/;
            zstd_retention_policy release_all;
        }

        location /bounded/ {
            alias %%TESTDIR%%/;
            zstd_retention_policy bounded 64k;
        }

        location /small_buffers/ {
            alias %%TESTDIR%%/;
            zstd_buffers 2 1k;
            zstd_parallel_tasks 1;
        }

        location /l19/ {
            alias %%TESTDIR%%/;
            zstd_comp_level 19;
        }

        # Each segment takes a while at level 22.
        location /slow/ {
            alias %%TESTDIR%%/;
            zstd_comp_level 22;
            zstd_parallel_segment_size 1m;
        }

        # Compressed in the event loop: the parallel compressor does not
        # take dictionaries.
        location /dict/ {
            alias %%TESTDIR%%/;
            zstd_dict_file %%TESTDIR%%/app.js;
        }

        location /proxy/ {
            proxy_pass http://127.0.0.1:8081/;
            proxy_buffer_size 4k;
            proxy_buffers 4 4k;
            proxy_busy_buffers_size 4k;
        }

        # Every buffer read from the upstream carries the flush flag.
        location /unbuffered/ {
            proxy_pass http://127.0.0.1:8081/;
            proxy_buffering off;
            proxy_buffer_size 16k;
            proxy_buffers 4 16k;
        }

        location /ssi/ {
            ssi on;
        }
    }

    server {
        listen       127.0.0.1:8081;
        server_name  localhost;

        zstd off;

        location / {
            root %%TESTDIR%%;
        }
    }
}

EOF

my $html = '<html>' . join(' ', map { "word$_" } 1 .. 60000) . '</html>';
my $js = "function f() { return 1; }\n" x 500;

$t->write_file('index.html', $html);
$t->write_file('app.js', $js);
# Pseudo-random words, so that each segment takes long enough at level 22
# for the client below to go away while they are being compressed.
my $seed = 7;
$t->write_file('big.html', join(' ', map {
	$seed = ($seed * 1103515245 + 12345) % 2147483648;
	'w' . ($seed >> 8) % 1000003
} 1 .. 1000000));
mkdir($t->testdir() . '/ssi');
$t->write_file('ssi/page.html',
	'<html><!--# include virtual="/app.js" -->|'
	. '<!--# include virtual="/index.html" --></html>');

$t->run();

my $testdir = $t->testdir();

###############################################################################

my $r;

cmp_ok(length($html), '>', 6 * 65536, 'several segments');

$r = get('/index.html', 'zstd');
like($r, qr/^Content-Encoding: zstd/mi, 'compressed');
is(zstd_body($r), $html, 'segments');

$r = get('/app.js', 'zstd');
is(zstd_body($r), $js, 'one segment');

$r = get('/serial/index.html', 'zstd');
is(zstd_body($r), $html, 'thread pool off');

$r = get('/default/index.html', 'zstd');
is(zstd_body($r), $html, 'default thread pool');

for my $n (1, 2) {
	$r = get('/noreuse/index.html', 'zstd');
	is(zstd_body($r), $html, "no reuse $n");
}

# Idle compressors keep no segment encoders, or only small ones.

for my $n (1, 2) {
	$r = get('/release/index.html', 'zstd');
	is(zstd_body($r), $html, "release_all retention $n");
}

for my $n (1, 2) {
	$r = get('/bounded/index.html', 'zstd');
	is(zstd_body($r), $html, "bounded retention $n");
}

$r = get('/small_buffers/index.html', 'zstd');
is(zstd_body($r), $html, 'small buffers, one task');

$r = get('/l19/index.html', 'zstd');
is(zstd_body($r), $html, 'comp level 19');

$r = get('/dict/index.html', 'zstd');
is(zstd_body($r, 'app.js'), $html, 'dictionary');

$r = get('/proxy/index.html', 'zstd');
is(zstd_body($r), $html, 'proxied');

$r = get('/proxy/app.js', 'zstd');
is(zstd_body($r), $js, 'proxied, one segment');

$r = get('/unbuffered/index.html', 'zstd');
is(zstd_body($r), $html, 'unbuffered proxied');

# The last segment is often delivered before the event loop reports its
# task: the response stays buffered until then ("no alerts" below).

my $repeated = 0;
for (1 .. 10) {
	$repeated++ if zstd_body(get('/index.html', 'zstd')) eq $html;
	$repeated++ if zstd_body(get('/proxy/index.html', 'zstd')) eq $html;
}
is($repeated, 20, 'repeated responses');

$r = get('/ssi/page.html', 'zstd');
is(zstd_body($r), '<html>' . $js . '|' . $html . '</html>', 'ssi subrequests');

$r = http(<<EOF);
HEAD /index.html HTTP/1.1
Host: localhost
Connection: close
Accept-Encoding: zstd

EOF
like($r, qr/^Content-Encoding: zstd/mi, 'head');
like($r, qr/\x0d\x0a\x0d\x0a$/, 'head no body');

# keepalive: two responses on one connection

$r = http(<<EOF);
GET /index.html HTTP/1.1
Host: localhost
Accept-Encoding: zstd

GET /app.js HTTP/1.1
Host: localhost
Connection: close
Accept-Encoding: zstd

EOF
my @parts = split /(?=HTTP\/1\.1 200)/, $r;
is(scalar @parts, 2, 'keepalive responses');
like($parts[1], qr/^Content-Encoding: zstd/mi, 'keepalive second compressed');

# a client that goes away while its segments are being encoded

for (1 .. 2) {
	my $s = IO::Socket::INET->new('127.0.0.1:' . port(8080))
		or die "Can't connect to nginx: $!\n";
	print $s "GET /slow/big.html HTTP/1.1\r\nHost: localhost\r\n"
		. "Accept-Encoding: zstd\r\n\r\n";
	select undef, undef, undef, 0.2;
	close $s;
}

$r = get('/index.html', 'zstd');
is(zstd_body($r), $html, 'after aborted requests');

$t->stop();

my $log = $t->read_file('ratio.log');
like($log, qr!^/index\.html \d+\.\d\d\d$!m, 'zstd_ratio');

SKIP: {
skip 'no debug log', 2 unless $t->has_module('--with-debug');

like($t->read_file('error.log'), qr/http zstd thread task done/,
	'tasks ran on the thread pool');
like($t->read_file('error.log'), qr/http terminate cleanup count:\d+ blk:1/,
	'aborted while tasks ran');

}

###############################################################################

sub get {
	my ($uri, $ae) = @_;
	my $header = defined $ae ? "Accept-Encoding: $ae\n" : '';
	return http(<<EOF);
GET $uri HTTP/1.1
Host: localhost
Connection: close
$header
EOF
}

sub zstd_body {
	my ($r, $dict) = @_;
	my $body = http_content($r);
	return undef unless defined $body;

	my $file = "$testdir/response.zst";
	open my $fh, '>', $file or die "Can't create $file: $!";
	binmode $fh;
	print $fh $body;
	close $fh;

	my $opts = defined $dict ? "-D $testdir/$dict" : '';
	return `zstd -dcq $opts $file`;
}

###############################################################################
