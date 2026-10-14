#!/usr/bin/perl

# Tests for ngx_http_zstd_filter_module.

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

my $t = Test::Nginx->new()->has(qw/http proxy ssi gzip/)->has_daemon('zstd')->plan(50)
	->write_file_expand('nginx.conf', <<'EOF');

%%TEST_GLOBALS%%

daemon off;

events {
}

http {
    %%TEST_GLOBALS_HTTP%%

    types {
        text/html               html;
        text/css                css;
        application/javascript  js;
    }

    log_format ratio '$uri $zstd_ratio';

    # Static files reach body filters as file buffers unless the filter asks
    # for them in memory.
    sendfile on;

    gzip on;
    gzip_types application/javascript;

    server {
        listen       127.0.0.1:8080;
        server_name  localhost;

        zstd on;
        zstd_types text/plain application/javascript;

        access_log %%TESTDIR%%/ratio.log ratio;

        location / {
        }

        location /off/ {
            alias %%TESTDIR%%/;
            zstd off;
        }

        location /l19/ {
            alias %%TESTDIR%%/;
            zstd_comp_level 19;
        }

        location /l22/ {
            alias %%TESTDIR%%/;
            zstd_comp_level 22;
        }

        location /negative/ {
            alias %%TESTDIR%%/;
            zstd_comp_level -5;
            zstd_window 1k;
            zstd_buffers 2 1k;
        }

        # SSI clears Content-Length: the length is unknown.
        location /unknown_l22/ {
            alias %%TESTDIR%%/;
            zstd_comp_level 22;
            ssi on;
        }

        location /min/ {
            alias %%TESTDIR%%/;
            zstd_min_length 1000;
        }

        location /noreuse/ {
            alias %%TESTDIR%%/;
            zstd_reuse_memory off;
        }

        location /aggressive/ {
            alias %%TESTDIR%%/;
            zstd_retention_policy aggressive;
        }

        location /current_config/ {
            alias %%TESTDIR%%/;
            zstd_retention_policy current_config;
            zstd_comp_level 9;
        }

        location /release_all/ {
            alias %%TESTDIR%%/;
            zstd_retention_policy release_all;
        }

        location /bounded/ {
            alias %%TESTDIR%%/;
            zstd_retention_policy bounded 64k;
            zstd_comp_level 19;
        }

        location /proxy/ {
            proxy_pass http://127.0.0.1:8081/;
        }

        # Every buffer read from the upstream carries the flush flag.
        location /unbuffered/ {
            proxy_pass http://127.0.0.1:8081/;
            proxy_buffering off;
            proxy_buffer_size 1k;
        }

        location /cond/ {
            alias %%TESTDIR%%/;
            if ($arg_nozstd) {
                zstd off;
            }
        }

        location /ssi/ {
            ssi on;
        }

        location = /404 {
            return 404 "not found, not found, not found, not found";
        }

        location = /500 {
            return 500 "internal error, internal error, internal error";
        }

        location = /redirect {
            error_page 404 = /index.html;
            return 404;
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

my $html = '<html>' . join(' ', map { "word$_" } 1 .. 20000) . '</html>';
my $js = "function f() { return 1; }\n" x 5000;

$t->write_file('index.html', $html);
$t->write_file('app.js', $js);
$t->write_file('style.css', 'body { color: red; }' x 100);
$t->write_file('tiny.html', '<p>');
$t->write_file('mid.html', 'X' x 500);

# Over 8 MB: levels 20 to 22 would size the window to 16 MB for its known
# length, and to 32-128 MB for an unknown one, more than the 8 MB browsers
# accept (RFC 9659).
my $chunk = join(' ', map { "w" . ($_ * 7919 % 100003) } 1 .. 200000);
my $big = $chunk;
$big .= $chunk while length($big) < 9 * 1024 * 1024;
$big .= $chunk;
$t->write_file('big.html', $big);
mkdir($t->testdir() . '/ssi');
$t->write_file('ssi/page.html',
	'<html><!--# include virtual="/app.js" -->|'
	. '<!--# include virtual="/mid.html" --></html>');

$t->run();

my $testdir = $t->testdir();

###############################################################################

my $r;

# negotiation

$r = get('/index.html', 'gzip, deflate, br, zstd');
like($r, qr/^Content-Encoding: zstd/mi, 'zstd accepted');
is(zstd_body($r), $html, 'html roundtrip');
unlike($r, qr/^Content-Length/mi, 'no content length');
like($r, qr/^ETag: W\//mi, 'weak etag');
unlike($r, qr/^Accept-Ranges/mi, 'no accept ranges');

$r = get('/index.html', 'ZSTD');
like($r, qr/^Content-Encoding: zstd/mi, 'zstd case insensitive');

$r = get('/index.html');
unlike($r, qr/^Content-Encoding/mi, 'no accept-encoding');
is(http_content($r), $html, 'no accept-encoding body');

$r = get('/index.html', 'gzip, zstd;q=0');
unlike($r, qr/^Content-Encoding: zstd/mi, 'zstd q=0 rejected');

$r = get('/index.html', 'zstd;q=0.001');
like($r, qr/^Content-Encoding: zstd/mi, 'zstd q=0.001 accepted');

$r = get('/index.html', 'zstdx');
unlike($r, qr/^Content-Encoding/mi, 'zstd token boundary');

$r = get('/index.html', 'gzip;q=0, br');
unlike($r, qr/^Content-Encoding/mi, 'br only');

# types and lengths

$r = get('/app.js', 'zstd');
like($r, qr/^Content-Encoding: zstd/mi, 'zstd_types match');
is(zstd_body($r), $js, 'js roundtrip');

$r = get('/style.css', 'zstd');
unlike($r, qr/^Content-Encoding/mi, 'zstd_types mismatch');

$r = get('/tiny.html', 'zstd');
unlike($r, qr/^Content-Encoding/mi, 'default min length');

$r = get('/min/mid.html', 'zstd');
unlike($r, qr/^Content-Encoding/mi, 'zstd_min_length');

$r = get('/mid.html', 'zstd');
like($r, qr/^Content-Encoding: zstd/mi, 'above min length');

# window: decoded within the 8 MB browsers allow (zstd -M8MB rejects a
# frame whose window is larger, as Chrome does)

for my $prefix (qw/l19 l22 unknown_l22/) {
	$r = get("/$prefix/big.html", 'zstd');
	ok(zstd_body($r, '-M8MB') eq $big, "$prefix: large response, 8 MB window");
}

# without a known length, the C library's level 22 declares 128 MB however
# short the response is, unless it all comes in the first call; subrequests
# deliver it in parts

$r = get('/unknown_l22/ssi/page.html', 'zstd');
is(zstd_body($r, '-M8MB'), '<html>' . $js . '|' . 'X' x 500 . '</html>',
	'unknown_l22: short response in parts, 8 MB window');

$r = get('/negative/index.html', 'zstd');
is(zstd_body($r, '-M1KB'), $html, 'negative level, 1k window, small buffers');

# configuration

$r = get('/off/index.html', 'zstd');
unlike($r, qr/^Content-Encoding/mi, 'zstd off');

$r = get('/cond/index.html', 'zstd');
like($r, qr/^Content-Encoding: zstd/mi, 'if: zstd inherited');

$r = get('/cond/index.html?nozstd=1', 'zstd');
unlike($r, qr/^Content-Encoding/mi, 'if: zstd off');

# memory reuse and retention policies: each request twice, so that the
# second one gets an encoder returned to the cache by the first

for my $prefix (qw/noreuse aggressive current_config release_all bounded/) {
	for my $n (1, 2) {
		$r = get("/$prefix/index.html", 'zstd');
		is(zstd_body($r), $html, "$prefix $n");
	}
}

# response kinds

$r = get('/proxy/index.html', 'zstd');
is(zstd_body($r), $html, 'proxied response');

$r = get('/unbuffered/index.html', 'zstd');
is(zstd_body($r), $html, 'unbuffered proxied response');

$r = get('/404', 'zstd');
like($r, qr/^Content-Encoding: zstd/mi, '404 compressed');
is(zstd_body($r), 'not found, not found, not found, not found', '404 body');

$r = get('/500', 'zstd');
unlike($r, qr/^Content-Encoding/mi, '500 not compressed');

$r = get('/redirect', 'zstd');
is(zstd_body($r), $html, 'error_page redirect');

$r = http(<<EOF);
HEAD /index.html HTTP/1.1
Host: localhost
Connection: close
Accept-Encoding: zstd

EOF
like($r, qr/^Content-Encoding: zstd/mi, 'head');
like($r, qr/\x0d\x0a\x0d\x0a$/, 'head no body');

$r = get('/ssi/page.html', 'zstd');
is(zstd_body($r), '<html>' . $js . '|' . 'X' x 500 . '</html>', 'ssi subrequests');

# gzip coexistence

$r = get('/app.js', 'gzip, zstd');
like($r, qr/^Content-Encoding: zstd/mi, 'zstd preferred over gzip');
unlike($r, qr/^Content-Encoding: gzip/mi, 'no double encoding');

$r = get('/app.js', 'gzip');
like($r, qr/^Content-Encoding: gzip/mi, 'gzip when zstd not accepted');

# $zstd_ratio

$t->stop();

my $log = $t->read_file('ratio.log');
like($log, qr!^/app\.js \d+\.\d\d\d$!m, 'zstd_ratio');
like($log, qr!^/style\.css -$!m, 'zstd_ratio not found');

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
	my ($r, $opts) = @_;
	$opts //= '';
	my $body = http_content($r);
	return undef unless defined $body;

	my $file = "$testdir/response.zst";
	open my $fh, '>', $file or die "Can't create $file: $!";
	binmode $fh;
	print $fh $body;
	close $fh;

	return `zstd -dcq $opts $file`;
}

###############################################################################
