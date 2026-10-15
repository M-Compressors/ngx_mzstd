#!/usr/bin/perl

# Tests for ngx_http_zstd_filter_module with zstd_dict_file: responses are
# compressed with a dictionary, which decoders need.

###############################################################################

use warnings;
use strict;

use Test::More;

BEGIN { use FindBin; chdir($FindBin::Bin); }

use lib 'lib';
use Test::Nginx qw/ :DEFAULT http_content /;

###############################################################################

select STDERR; $| = 1;
select STDOUT; $| = 1;

my $t = Test::Nginx->new()->has(qw/http ssi/)->has_daemon('zstd')->plan(21)
	->write_file_expand('nginx.conf', <<'EOF');

%%TEST_GLOBALS%%

daemon off;

events {
}

http {
    %%TEST_GLOBALS_HTTP%%

    types {
        text/html         html;
        application/json  json;
    }

    zstd on;
    zstd_types application/json;

    # Relative to the configuration prefix, at the http level, as with
    # tokers/zstd-nginx-module.
    zstd_dict_file raw.dict;

    server {
        listen       127.0.0.1:8080;
        server_name  localhost;

        location / {
        }

        location /l19/ {
            alias %%TESTDIR%%/;
            zstd_comp_level 19;
        }

        location /trained/ {
            alias %%TESTDIR%%/;
            zstd_dict_file %%TESTDIR%%/trained.dict;
        }

        location /off/ {
            alias %%TESTDIR%%/;
            zstd_dict_file off;
        }

        # SSI clears Content-Length: the length is unknown.
        location /ssi/ {
            alias %%TESTDIR%%/;
            ssi on;
            ssi_types application/json;
        }
    }
}

EOF

my $testdir = $t->testdir();

# JSON records that share most of their text.
sub records {
	my ($seed, $count) = @_;
	return '[' . join(',', map {
		my $n = $seed * 31 + $_;
		qq({"id":$n,"user":"user) . ($n * 7 % 97)
			. qq(","status":"active","roles":["reader","writer"]})
	} 1 .. $count) . ']';
}

my $json = records(1000, 50);
$t->write_file('data.json', $json);
# Where a dictionary pays off: a response too short to repeat itself much.
my $small = records(1000, 3);
$t->write_file('small.json', $small);

# A raw-content dictionary, and a Zstandard one trained on samples.
$t->write_file('raw.dict', records(1, 20));
mkdir("$testdir/samples");
$t->write_file("samples/$_.json", records($_, 8)) for 1 .. 200;
system("zstd -q --train $testdir/samples/*.json --maxdict=4096 "
	. "-o $testdir/trained.dict") == 0
	or die "Can't train a dictionary";

$t->run();

###############################################################################

my ($r, $plain);

$r = get('/data.json');
like($r, qr/^Content-Encoding: zstd/mi, 'compressed');
is(zstd_body($r, 'raw.dict'), $json, 'decoded with the dictionary');
isnt(zstd_body($r), $json, 'not decoded without it');
is(dict_id($r), 0, 'raw content: no dictionary id');

$plain = get('/off/data.json');
like($plain, qr/^Content-Encoding: zstd/mi, 'off: compressed');
is(zstd_body($plain), $json, 'off: decoded without a dictionary');

$r = get('/small.json');
$plain = get('/off/small.json');
is(zstd_body($r, 'raw.dict'), $small, 'short response');
ok(length(http_content($r)) < length(http_content($plain)),
	'short response: smaller with the dictionary');

# the encoder that compressed without the dictionary is reused with it
$r = get('/data.json');
is(zstd_body($r, 'raw.dict'), $json, 'reused encoder');

$r = get('/l19/data.json');
is(zstd_body($r, 'raw.dict'), $json, 'level 19');
$r = get('/l19/data.json');
is(zstd_body($r, 'raw.dict'), $json, 'level 19, reused encoder');

$r = get('/trained/data.json');
is(zstd_body($r, 'trained.dict'), $json, 'trained dictionary');
isnt(zstd_body($r, 'raw.dict'), $json, 'trained: not decoded with another');
isnt(dict_id($r), 0, 'trained: dictionary id');

$r = get('/ssi/data.json');
like($r, qr/^Content-Encoding: zstd/mi, 'unknown length: compressed');
unlike($r, qr/^Content-Length/mi, 'unknown length');
is(zstd_body($r, 'raw.dict'), $json, 'unknown length: decoded');

$r = get('/data.json', 'gzip');
unlike($r, qr/^Content-Encoding/mi, 'zstd not accepted');
is(http_content($r), $json, 'zstd not accepted: body');

# text/html, which is always compressed

$t->write_file('page.html', 'X' x 500);
$r = get('/page.html');
like($r, qr/^Content-Encoding: zstd/mi, 'text/html compressed');
is(zstd_body($r, 'raw.dict'), 'X' x 500, 'text/html decoded');

###############################################################################

sub get {
	my ($uri, $ae) = @_;
	$ae //= 'zstd';
	return http(<<EOF);
GET $uri HTTP/1.1
Host: localhost
Connection: close
Accept-Encoding: $ae

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
	return `zstd -dcq $opts $file 2>/dev/null`;
}

# The Dictionary_ID_flag of the frame header descriptor: 0 when the frame
# names no dictionary.
sub dict_id {
	my ($r) = @_;
	return ord(substr(http_content($r), 4, 1)) & 3;
}

###############################################################################
