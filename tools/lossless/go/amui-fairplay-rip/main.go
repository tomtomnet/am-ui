// amui-fairplay-rip: fetch and decrypt one FairPlay-protected (cbcs) HLS audio rendition through the
// local wrapper-lite service and leave a plain progressive MP4 (.m4a) behind.
//
// tools/lossless/build.sh copies this file into the apple-music-downloader module copy
// (var/build/lossless/amdl-src/cmd/amui-fairplay-rip) and builds it there, because it reuses that
// module's internal packages: fairplay-rip (streamed download + pure-Go Temari decrypt driven by the
// wrapper's /key templates), media/defrag (fMP4 -> progressive MP4) and media/alacfix (repairs the
// end marker some Apple ALAC packets lack). None of the downloader's catalog, tagging or
// update-check code is linked in.
//
//	amui-fairplay-rip --lite http://127.0.0.1:12351 --adam 1234567890 --out song.m4a [--max-memory 256] <url
//
// The media playlist URL (the rendition's #EXT-X-STREAM-INF URI) is read from standard input as one
// line, so signed URLs never show up in argv or /proc. The ripper's progress goes to standard output.
// On failure the exit status is non-zero and standard error carries one line with URLs redacted.
// Output files are created 0600 (umask 077).
package main

import (
	"bufio"
	"errors"
	"flag"
	"fmt"
	"io"
	"net/url"
	"os"
	"regexp"
	"strings"
	"syscall"

	"amdl/internal/config"
	"amdl/internal/download"
	fairplayrip "amdl/internal/fairplay-rip"
	"amdl/internal/media/alacfix"
	fmp4unfrag "amdl/internal/media/defrag"
)

const version = "amui-fairplay-rip 1 (apple-music-downloader 0.1.4 internals)"

var urlPattern = regexp.MustCompile(`https?://[^\s"'<>]+`)

func redact(err error) string {
	return urlPattern.ReplaceAllString(err.Error(), "<url>")
}

func fail(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "amui-fairplay-rip: "+format+"\n", args...)
	os.Exit(1)
}

func readPlaylistURL(in io.Reader) (string, error) {
	reader := bufio.NewReaderSize(in, 64*1024)
	line, err := reader.ReadString('\n')
	if err != nil && !errors.Is(err, io.EOF) {
		return "", err
	}
	line = strings.TrimSpace(line)
	if line == "" {
		return "", errors.New("no playlist URL on standard input")
	}
	parsed, err := url.Parse(line)
	if err != nil {
		return "", errors.New("the playlist URL does not parse")
	}
	if parsed.Scheme != "https" {
		return "", errors.New("the playlist URL must use https")
	}
	host := strings.ToLower(parsed.Hostname())
	if host != "apple.com" && !strings.HasSuffix(host, ".apple.com") && !strings.HasSuffix(host, ".mzstatic.com") {
		return "", fmt.Errorf("the playlist host %q is not an Apple CDN host", host)
	}
	return line, nil
}

func main() {
	syscall.Umask(0o077)
	lite := flag.String("lite", "", "wrapper-lite base URL (http://127.0.0.1:PORT)")
	adam := flag.String("adam", "", "numeric song (adam) id")
	out := flag.String("out", "", "output .m4a path (created 0600; must not exist)")
	maxMemory := flag.Int("max-memory", 256, "MiB: media up to this size is assembled in memory, larger files stream through <out>.part")
	showVersion := flag.Bool("version", false, "print the version and exit")
	flag.Parse()
	if *showVersion {
		fmt.Println(version)
		return
	}
	if *lite == "" || *adam == "" || *out == "" {
		fail("--lite, --adam and --out are required")
	}
	for _, r := range *adam {
		if r < '0' || r > '9' {
			fail("--adam must be numeric")
		}
	}
	liteURL, err := url.Parse(*lite)
	if err != nil || liteURL.Scheme != "http" || liteURL.Hostname() != "127.0.0.1" {
		fail("--lite must be an http://127.0.0.1:PORT URL (the wrapper binds loopback only)")
	}
	if _, err := os.Lstat(*out); err == nil {
		fail("output file already exists")
	}
	playlist, err := readPlaylistURL(os.Stdin)
	if err != nil {
		fail("%s", err)
	}
	if err := download.Init(""); err != nil {
		fail("http client: %s", redact(err))
	}

	cfg := config.ConfigSet{}
	cfg.General.LiteServer = strings.TrimRight(*lite, "/")
	cfg.General.MaxMemoryLimit = *maxMemory
	if err := fairplayrip.Run(*adam, playlist, *out, cfg); err != nil {
		os.Remove(*out)
		os.Remove(*out + ".part")
		fail("rip: %s", redact(err))
	}
	if err := fmp4unfrag.DefragmentMP4(*out); err != nil {
		os.Remove(*out)
		fail("defragment: %s", redact(err))
	}
	result, err := alacfix.Fix(*out, false)
	if err != nil {
		os.Remove(*out)
		fail("alac fix: %s", redact(err))
	}
	if result.TracksCount > 0 {
		fmt.Printf("alacfix: %d packet(s) repaired\n", result.Patched)
	}
	info, err := os.Stat(*out)
	if err != nil || info.Size() == 0 {
		os.Remove(*out)
		fail("no output produced")
	}
	if err := os.Chmod(*out, 0o600); err != nil {
		fail("chmod: %s", err)
	}
	fmt.Printf("done %d\n", info.Size())
}
