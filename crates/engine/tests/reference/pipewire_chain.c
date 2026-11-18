/* What PipeWire's own filter code makes of a 16-channel signal in the graph of the sink
 * "Atmos 9.1.6 Downmix": the reference of crates/engine/src/dsp.rs.
 *
 * This file holds none of PipeWire's code. It includes two files of PipeWire's source tree
 * (version 1.6.11: spa/plugins/audioconvert/biquad.c for the coefficients, and the plain C
 * loops at the top of spa/plugins/filter-graph/audio-dsp-c.c) and connects them the way
 * workspace/pipewire/atmos-916-downmix.conf connects the sink's nodes, with the control logic of
 * the built-in nodes (mixer, delay, linear, bq_*) of spa/plugins/filter-graph/plugin_builtin.c.
 * pipewire_chain.sh fetches the two files and builds it.
 *
 *   pipewire_chain FRAMES GAIN1..GAIN8 LFE LOWPASS Q1 Q2 DELAY PREAMP EQGAIN [KIND FREQ Q GAIN]... > out.f32
 *
 * KIND is lsc, pk or hsc. The input is made here: sample n of channel c is
 * ((7 n + 13 c) mod 251) / 251 - 0.5, silent between frames 20000 and 30000 (the filters' states
 * decay there). The output is interleaved stereo, 32-bit floats. Blocks are 1024 frames, the
 * sink's quantum.
 */
#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define SPA_RESTRICT restrict
#define spa_memcpy memcpy
#define SPA_CLAMP(v, low, high) ((v) > (high) ? (high) : ((v) < (low) ? (low) : (v)))
#define SPA_ROUND_UP_N(num, align) ((((num) - 1) | ((align) - 1)) + 1)

#include "biquad.c"
#include "audio-dsp-part.inc"

#define RATE 48000ul
#define QUANTUM 1024
#define CHANNELS 16

/* mixer_run of plugin_builtin.c: inputs without a connection or with gain 0 are left out, equal
 * gains are applied once after the sum */
static void mixer(float *out, const float *in[8], const float gain[8], uint32_t samples)
{
	const float *src[8];
	float gains[8];
	int i, n_src = 0;
	bool eq_gain = true;

	for (i = 0; i < 8; i++) {
		if (in[i] == NULL || gain[i] == 0.0f)
			continue;
		src[n_src] = in[i];
		gains[n_src++] = gain[i];
		if (gain[i] != gains[0])
			eq_gain = false;
	}
	if (eq_gain)
		dsp_mix_gain_c(NULL, out, src, n_src, gains, 1, samples);
	else
		dsp_mix_gain_c(NULL, out, src, n_src, gains, n_src, samples);
}

struct delay {
	float *buffer;
	uint32_t ptr, buffer_samples, delay_samples;
};

/* delay_instantiate and delay_run of plugin_builtin.c ("max-delay" = 0.1) */
static void delay_init(struct delay *d, float seconds)
{
	d->buffer_samples = SPA_ROUND_UP_N((uint32_t)(0.1f * RATE), 64);
	d->buffer = calloc(d->buffer_samples * 2 + 64, sizeof(float));
	d->ptr = 0;
	d->delay_samples = SPA_CLAMP((uint32_t)(seconds * RATE), 0u, d->buffer_samples - 1);
}

/* bq_freq_update of plugin_builtin.c */
static void bq_init(struct biquad *bq, int type, float freq, float Q, float gain)
{
	biquad_set(bq, type, freq * 2 / RATE, Q, gain);
}

int main(int argc, char **argv)
{
	if (argc < 17 || (argc - 17) % 4 != 0) {
		fprintf(stderr, "usage: see the top of pipewire_chain.c\n");
		return 2;
	}
	uint32_t frames = (uint32_t)strtoul(argv[1], NULL, 10);
	float gains[8], out_gains[8] = { 1.0f };
	for (int i = 0; i < 8; i++)
		gains[i] = strtof(argv[2 + i], NULL);
	out_gains[1] = strtof(argv[10], NULL);
	float lowpass = strtof(argv[11], NULL), q1 = strtof(argv[12], NULL), q2 = strtof(argv[13], NULL);
	float delay_s = strtof(argv[14], NULL), preamp = strtof(argv[15], NULL), eqgain = strtof(argv[16], NULL);
	int n_eq = (argc - 17) / 4;

	struct biquad lp1, lp2, *eq_l = calloc(n_eq + 1, sizeof(*eq_l)), *eq_r = calloc(n_eq + 1, sizeof(*eq_r));
	bq_init(&lp1, BQ_LOWPASS, lowpass, q1, 0.0f);
	bq_init(&lp2, BQ_LOWPASS, lowpass, q2, 0.0f);
	for (int i = 0; i < n_eq; i++) {
		const char *kind = argv[17 + 4 * i];
		int type = strcmp(kind, "lsc") == 0 ? BQ_LOWSHELF : strcmp(kind, "hsc") == 0 ? BQ_HIGHSHELF : BQ_PEAKING;
		float freq = strtof(argv[18 + 4 * i], NULL), q = strtof(argv[19 + 4 * i], NULL);
		float gain = strtof(argv[20 + 4 * i], NULL);
		bq_init(&eq_l[i], type, freq, q, gain);
		bq_init(&eq_r[i], type, freq, q, gain);
	}
	struct delay dly_l, dly_r;
	delay_init(&dly_l, delay_s);
	delay_init(&dly_r, delay_s);

	static float in[CHANNELS][QUANTUM], c[QUANTUM], dry_l[QUANTUM], dry_r[QUANTUM], lfe1[QUANTUM], lfe2[QUANTUM];
	static float del_l[QUANTUM], del_r[QUANTUM], out_l[QUANTUM], out_r[QUANTUM], interleaved[2 * QUANTUM];
	/* inputs = [ dry_l:In 1, dry_r:In 1, c:In, lfe_lp1:In, dry_l:In 3, dry_r:In 3, ... dry_l:In 8, dry_r:In 8 ] */
	const float *left[8] = { in[0], c, in[4], in[6], in[8], in[10], in[12], in[14] };
	const float *right[8] = { in[1], c, in[5], in[7], in[9], in[11], in[13], in[15] };
	const float *sum_l[8] = { del_l, lfe2 }, *sum_r[8] = { del_r, lfe2 };

	for (uint32_t done = 0; done < frames; done += QUANTUM) {
		uint32_t n = frames - done < QUANTUM ? frames - done : QUANTUM;
		for (uint32_t i = 0; i < n; i++) {
			uint64_t frame = done + i;
			bool silent = frame >= 20000 && frame < 30000;
			for (int ch = 0; ch < CHANNELS; ch++)
				in[ch][i] = silent ? 0.0f : (float)(((frame * 7 + ch * 13) % 251) / 251.0 - 0.5);
		}
		dsp_copy_c(NULL, c, in[2], n);                         /* c: copy of FC */
		mixer(dry_l, left, gains, n);
		mixer(dry_r, right, gains, n);
		biquad_run_c(NULL, &lp1, lfe1, in[3], n);
		biquad_run_c(NULL, &lp2, lfe2, lfe1, n);
		dsp_delay_c(NULL, dly_l.buffer, &dly_l.ptr, dly_l.buffer_samples, dly_l.delay_samples, del_l, dry_l, n, 0.0f, 0.0f);
		dsp_delay_c(NULL, dly_r.buffer, &dly_r.ptr, dly_r.buffer_samples, dly_r.delay_samples, del_r, dry_r, n, 0.0f, 0.0f);
		mixer(out_l, sum_l, out_gains, n);
		mixer(out_r, sum_r, out_gains, n);
		dsp_linear_c(NULL, out_l, out_l, preamp, 0.0f, n);     /* preamp_l, preamp_r */
		dsp_linear_c(NULL, out_r, out_r, preamp, 0.0f, n);
		dsp_linear_c(NULL, out_l, out_l, eqgain, 0.0f, n);     /* eqgain_l, eqgain_r */
		dsp_linear_c(NULL, out_r, out_r, eqgain, 0.0f, n);
		for (int i = 0; i < n_eq; i++) {
			biquad_run_c(NULL, &eq_l[i], out_l, out_l, n);
			biquad_run_c(NULL, &eq_r[i], out_r, out_r, n);
		}
		for (uint32_t i = 0; i < n; i++) {
			interleaved[2 * i] = out_l[i];
			interleaved[2 * i + 1] = out_r[i];
		}
		fwrite(interleaved, sizeof(float), 2 * n, stdout);
	}
	return 0;
}
