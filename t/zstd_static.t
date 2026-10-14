#!/usr/bin/perl

# Tests for ngx_http_zstd_static_module.

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

my $t = Test::Nginx->new()->has(qw/http gzip/)->plan(20)
	->write_file_expand('nginx.conf', <<'EOF');

%%TEST_GLOBALS%%

daemon off;

events {
}

http {
    %%TEST_GLOBALS_HTTP%%

    server {
        listen       127.0.0.1:8080;
        server_name  localhost;

        location / {
            zstd_static on;
        }

        location /always/ {
            alias %%TESTDIR%%/;
            zstd_static always;
        }

        location /off/ {
            alias %%TESTDIR%%/;
            zstd_static off;
        }

        location /vary/ {
            alias %%TESTDIR%%/;
            zstd_static on;
            gzip_vary on;
        }
    }
}

EOF

$t->write_file('file.txt', 'original');
$t->write_file('file.txt.zst', 'compressed');
$t->write_file('plain.txt', 'no precompressed variant');
mkdir($t->testdir() . '/dir');
$t->write_file('dir/index.html', 'index');

$t->run();

###############################################################################

my $r;

$r = get('/file.txt', 'zstd');
like($r, qr/^Content-Encoding: zstd/mi, 'on: encoding');
like($r, qr/^Content-Length: 10\x0d?$/mi, 'on: length');
like($r, qr/^Content-Type: text\/plain/mi, 'on: type of original');
like($r, qr/\x0d\x0a\x0d\x0acompressed$/, 'on: body');

$r = get('/file.txt', 'gzip');
unlike($r, qr/^Content-Encoding/mi, 'on: not accepted');
like($r, qr/\x0d\x0a\x0d\x0aoriginal$/, 'on: original body');

$r = get('/file.txt', 'zstd;q=0');
unlike($r, qr/^Content-Encoding/mi, 'on: q=0');

$r = get('/plain.txt', 'zstd');
like($r, qr/\x0d\x0a\x0d\x0ano precompressed variant$/, 'on: no .zst file');

$r = get('/dir/', 'zstd');
like($r, qr/\x0d\x0a\x0d\x0aindex$/, 'on: directory');

$r = get('/always/file.txt');
like($r, qr/^Content-Encoding: zstd/mi, 'always: encoding');
like($r, qr/\x0d\x0a\x0d\x0acompressed$/, 'always: body');

$r = get('/off/file.txt', 'zstd');
unlike($r, qr/^Content-Encoding/mi, 'off');

$r = http(<<EOF);
HEAD /file.txt HTTP/1.1
Host: localhost
Connection: close
Accept-Encoding: zstd

EOF
like($r, qr/^Content-Encoding: zstd/mi, 'head: encoding');
like($r, qr/\x0d\x0a\x0d\x0a$/, 'head: no body');

# Vary is added only with gzip_vary, and only when the .zst file exists

$r = get('/file.txt', 'zstd');
unlike($r, qr/^Vary/mi, 'no gzip_vary: no vary');

$r = get('/vary/file.txt', 'zstd');
like($r, qr/^Vary: Accept-Encoding/mi, 'gzip_vary: vary');

$r = get('/vary/file.txt', 'gzip');
like($r, qr/^Vary: Accept-Encoding/mi, 'gzip_vary: vary when not accepted');
like($r, qr/\x0d\x0a\x0d\x0aoriginal$/, 'gzip_vary: original body');

$r = get('/vary/plain.txt', 'zstd');
unlike($r, qr/^Vary/mi, 'gzip_vary: no vary without .zst file');

$r = get('/file.txt', 'gzip, zstd');
like($r, qr/^Content-Encoding: zstd/mi, 'zstd among others');

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

###############################################################################
