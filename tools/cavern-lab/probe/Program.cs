// am-ui cavern-lab probe: the workspace CavernProbe (builds/music-downloader/cavern-probe/Program.cs) plus
// experiment switches and instrumentation. With no options the output is bit-identical to the workspace probe:
//   CavernProbe input.m4a output-directory
// Options (all optional, after the two positionals):
//   --mode render|bed          bed: decode the E-AC-3 core only (no JOC, no renderer) to bed.f32le
//   --update-rate N            Listener.UpdateRate (default 32 = one JOC timeslot / 2)
//   --layout refNNN            ChannelPrototype layout to render to (default ref916; e.g. ref714)
//   --no-lfe-separation        Listener.LFESeparation = false
//   --no-direct-lfe            Listener.DirectLFE = false
//   --normalizer F             Listener.Normalizer (default 0 = off)
//   --lfe-gain F               extra gain on the LFE output channel(s) of the render
//   --lfe-delay N              delay the LFE output channel(s) by N samples (577: the QMF bank's delay, which the other
//                              channels have from the JOC reconstruction and the decoder's LFE does not)
//   --bass-manage GAIN FREQ    add GAIN x lowpass(FREQ, sum of the non-LFE output channels) to the LFE output (bass management)
//   --bass-type bw4|bw2|lr4|bw1  low-pass type for --bass-manage (default bw4 = 4th-order Butterworth)
//   --surround-phase -1|1      multiply the Ls/Rs QMF samples by -j/+j before JOC reconstruction when joc_dmx_config_idx is 3/4
//   --joc-interp-fix           spec-faithful (TS 103 420 6.6.5) matrix interpolation
//   --window START END         seconds; the logs/dumps below cover only this range (default 4.5 7.9)
//   --log-joc                  joc-log.jsonl: per-frame JOC parameters + mixing matrices computed in the window
//   --log-objects              objects-log.jsonl: per render block, position/volume/size of every object
//   --dump-objects             objects.f32le: raw per-object samples (before spatialisation) in the window
//   --joc-timeslot-fix         LabSwitches.JocTimeslotWrapAtFrame (wrap the applier's timeslot at the frame length)
//   --mute-sparse              LabSwitches.MuteSparseObjects (pre-#419 behaviour: sparse-coded objects are silent)
//   --sparse-gain F            LabSwitches.SparseGainScale (scale sparse-coded dequantization steps)
using System.Diagnostics;
using System.Globalization;
using System.Numerics;
using System.Reflection;
using System.Runtime.InteropServices;
using System.Text.Json;
using Cavern;
using Cavern.Channels;
using Cavern.Format;
using Cavern.Format.Decoders;
using Cavern.Format.Decoders.EnhancedAC3;
using Cavern.Format.Renderers;

// A failure ends the render with a line in its log and an exit status the engine reports: an
// unhandled exception would abort the process and leave a core dump.
AppDomain.CurrentDomain.UnhandledException += (_, failure) => {
    Exception? error = failure.ExceptionObject as Exception;
    Console.Error.WriteLine($"render failed: {error?.GetType().Name}: {error?.Message}");
    Environment.Exit(1);
};

CultureInfo inv = CultureInfo.InvariantCulture;
LabOptions opts = new();
List<string> positional = new();
bool selfTest = false;
for (int i = 0; i < args.Length; i++) {
    string a = args[i];
    string Next() => i + 1 < args.Length ? args[++i] : throw new Exception($"{a} needs a value");
    switch (a) {
        case "--self-test": selfTest = true; break;
        case "--mode": opts.Mode = Next(); break;
        case "--update-rate": opts.UpdateRate = int.Parse(Next(), inv); break;
        case "--layout": opts.Layout = Next(); break;
        case "--no-lfe-separation": opts.LfeSeparation = false; break;
        case "--no-direct-lfe": opts.DirectLfe = false; break;
        case "--normalizer": opts.Normalizer = float.Parse(Next(), inv); break;
        case "--lfe-gain": opts.LfeGain = float.Parse(Next(), inv); break;
        case "--lfe-delay": opts.LfeDelay = int.Parse(Next(), inv); if (opts.LfeDelay < 0) throw new Exception("--lfe-delay must not be negative"); break;
        case "--window": opts.WindowStart = double.Parse(Next(), inv); opts.WindowEnd = double.Parse(Next(), inv); break;
        case "--log-joc": opts.LogJoc = true; break;
        case "--log-objects": opts.LogObjects = true; break;
        case "--dump-objects": opts.DumpObjects = true; break;
        case "--dump-bed": opts.DumpBed = true; break;
        case "--joc-timeslot-fix": opts.JocTimeslotFix = true; break;
        case "--mute-sparse": opts.MuteSparse = true; break;
        case "--sparse-gain": opts.SparseGain = float.Parse(Next(), inv); break;
        case "--surround-phase": opts.SurroundPhase = int.Parse(Next(), inv); break;
        case "--joc-interp-fix": opts.JocInterpFix = true; break;
        case "--bass-manage": opts.BassManageGain = float.Parse(Next(), inv); opts.BassManageFreq = float.Parse(Next(), inv); break;
        case "--bass-type": opts.BassType = Next(); break;
        default:
            if (a.StartsWith("--")) throw new Exception($"Unknown option {a}");
            positional.Add(a);
            break;
    }
}

int[] permutation916 = [0, 1, 2, 3, 6, 7, 4, 5, 8, 9, 10, 11, 12, 13, 14, 15];
ReferenceChannel[] expected = [ReferenceChannel.FrontLeft, ReferenceChannel.FrontRight,
    ReferenceChannel.FrontCenter, ReferenceChannel.ScreenLFE, ReferenceChannel.SideLeft,
    ReferenceChannel.SideRight, ReferenceChannel.RearLeft, ReferenceChannel.RearRight,
    ReferenceChannel.WideLeft, ReferenceChannel.WideRight, ReferenceChannel.TopFrontLeft,
    ReferenceChannel.TopFrontRight, ReferenceChannel.TopSideLeft, ReferenceChannel.TopSideRight,
    ReferenceChannel.TopRearLeft, ReferenceChannel.TopRearRight];
if (!permutation916.Select(i => ChannelPrototype.ref916[i]).SequenceEqual(expected))
    throw new Exception("9.1.6 speaker-order mapping does not match the workspace route");
if (selfTest) {
    float[] labelled = Enumerable.Range(0, 16).Select(i => (float)i).ToArray();
    if (!permutation916.Select(i => labelled[i]).SequenceEqual(new float[] {0,1,2,3,6,7,4,5,8,9,10,11,12,13,14,15}))
        throw new Exception("Permutation test failed");
    Console.WriteLine("Reference-speaker and labelled-channel permutation checks passed.");
    return;
}
if (positional.Count != 2) throw new Exception("Usage: CavernProbe input.m4a output-directory [options]");
string input = positional[0];
string output = Path.GetFullPath(positional[1]);
Directory.CreateDirectory(output);
if (!BitConverter.IsLittleEndian) throw new Exception("This artifact requires little-endian float output");

LabSwitches.JocTimeslotWrapAtFrame = opts.JocTimeslotFix;
LabSwitches.MuteSparseObjects = opts.MuteSparse;
LabSwitches.SparseGainScale = opts.SparseGain;
LabSwitches.SurroundPhaseShift = opts.SurroundPhase;
LabSwitches.JocInterpolationFix = opts.JocInterpFix;

if (opts.Mode == "bed") {
    RunBedDecode(input, output, opts);
    return;
}
if (opts.Mode == "qmf-test") {
    RunQmfTest(output);
    return;
}
if (opts.Mode == "info") {
    Console.WriteLine("GetStandardMatrix(6): " + string.Join(' ', ChannelPrototype.GetStandardMatrix(6).Select(Short)));
    Console.WriteLine("JOC input order (JointObjectCodingTables.inputMatrix, internal): FL FR FC SL SR RL RR");
    using AudioReader infoReader = AudioReader.Open(input);
    infoReader.ReadHeader();
    using Renderer infoRenderer = infoReader.GetRenderer();
    Console.WriteLine("decoder GetChannels(): " + string.Join(' ', infoRenderer.GetChannels().Select(Short)));
    return;
}
if (opts.Mode != "render") throw new Exception("--mode must be render or bed");

using AudioReader reader = AudioReader.Open(input);
reader.ReadHeader();
using Renderer renderer = reader.GetRenderer();
if (renderer is not EnhancedAC3Renderer enhanced || !renderer.HasObjects)
    throw new Exception("Input did not initialize as object-based E-AC-3; refusing channel-only output");
if (reader.SampleRate != 48000 || reader.Length <= 0)
    throw new Exception("Expected a finite 48 kHz track");
// Observe the existing decoder state without patching the Cavern checkout.
const BindingFlags any = BindingFlags.Instance | BindingFlags.NonPublic | BindingFlags.Public;
var decoderField = typeof(Renderer).GetField("stream", any)!;
var decoder = (EnhancedAC3Decoder)decoderField.GetValue(renderer)!;
var extensionProperty = typeof(EnhancedAC3Decoder).GetProperty("Extensions", any)!;
object extensions = extensionProperty.GetValue(decoder)!;
object joc = extensions.GetType().GetProperty("JOC")!.GetValue(extensions)!;
object oamd = extensions.GetType().GetProperty("OAMD")!.GetValue(extensions)!;
int IntProperty(object instance, string property) => Convert.ToInt32(instance.GetType().GetProperty(property)!.GetValue(instance));
int jocObjects = IntProperty(joc, "ObjectCount"), oamdObjects = IntProperty(oamd, "ObjectCount");
if (jocObjects <= 0 || oamdObjects <= 0) throw new Exception("Missing JOC or OAMD object metadata");
object JocField(string name) => joc.GetType().GetField(name, any)!.GetValue(joc)!;
int beds = Convert.ToInt32(oamd.GetType().GetProperty("Beds")!.GetValue(oamd));
int lfePosition = Convert.ToInt32(oamd.GetType().GetMethod("GetLFEPosition")!.Invoke(oamd, null));
var objectSamplesField = typeof(Renderer).GetField("objectSamples", any)!;
object applier = typeof(EnhancedAC3Renderer).GetField("applier", any)!.GetValue(renderer)!;
var applierTimeslotField = applier.GetType().GetField("timeslot", any)!;
ReferenceChannel[] bedChannels = ((Func<ReferenceChannel[]>)(() => renderer.GetChannels()))();

ReferenceChannel[] layout = (ReferenceChannel[])(typeof(ChannelPrototype).GetField(opts.Layout, BindingFlags.Static | BindingFlags.Public)
    ?? throw new Exception($"Unknown layout {opts.Layout}")).GetValue(null)!;
int[] permutation = opts.Layout == "ref916" ? permutation916 : Enumerable.Range(0, layout.Length).ToArray();
int channels = layout.Length;
string[] names = permutation.Select(i => Short(layout[i])).ToArray();

Listener.HeadphoneVirtualizer = false;
Listener.EnvironmentType = Environments.Home;
Listener.EnvironmentSize = new Vector3(10, 7, 10);
Listener.ReplaceChannels(ChannelPrototype.ToLayout(layout));
int block = opts.UpdateRate; // Default: one JOC timeslot / 2, observe renderer fallback at this granularity.
Listener listener = new() { SampleRate = 48000, UpdateRate = block, Normalizer = opts.Normalizer,
    Volume = 1, LFEVolume = 1, LFESeparation = opts.LfeSeparation, DirectLFE = opts.DirectLfe };
listener.AttachSources(renderer.Objects);
double[] sums = new double[channels], peaks = new double[channels];
long[] nonzero = new long[channels], aboveOne = new long[channels];
float[] reordered = new float[block * channels];
long rendered = 0, objectBlocks = 0, fallbackBlocks = 0, nonfinite = 0;
int minJoc = jocObjects, maxJoc = jocObjects, minOamd = oamdObjects, maxOamd = oamdObjects;
bool[] lfeChannel = names.Select(n => n == "LFE").ToArray();
// Delay line of each LFE channel for --lfe-delay (the samples still to come out, oldest at lfeDelayAt).
float[]?[] lfeDelayLines = lfeChannel.Select(lfe => lfe && opts.LfeDelay > 0 ? new float[opts.LfeDelay] : null).ToArray();
int lfeDelayAt = 0;
// Low-pass for the bass-managed LFE feed: bw4 = 4th-order Butterworth (two RBJ biquads), bw2 = 2nd-order Butterworth,
// lr4 = Linkwitz-Riley 4th order, bw1 = first-order.
Biquad[] bassFilters = opts.BassManageGain == 0 ? Array.Empty<Biquad>() : opts.BassType switch {
    "bw4" => new[] { new Biquad(opts.BassManageFreq, 0.5411961f), new Biquad(opts.BassManageFreq, 1.3065630f) },
    "bw2" => new[] { new Biquad(opts.BassManageFreq, 0.70710678f) },
    "lr4" => new[] { new Biquad(opts.BassManageFreq, 0.70710678f), new Biquad(opts.BassManageFreq, 0.70710678f) },
    "bw1" => new[] { Biquad.FirstOrder(opts.BassManageFreq) },
    _ => throw new Exception("--bass-type must be bw4, bw2, lr4 or bw1") };
long framesWithTwoDataPoints = 0, framesWithSteep = 0;
int downmixConfig = Convert.ToInt32(joc.GetType().GetProperty("DownmixConfig")!.GetValue(joc));
Stopwatch watch = Stopwatch.StartNew();
string rawPath = Path.Combine(output, "atmos-916.f32le");
using FileStream pcm = new(rawPath, FileMode.CreateNew, FileAccess.Write, FileShare.Read, 1024 * 1024);
Console.WriteLine($"JOC detected: {jocObjects} JOC objects, {oamdObjects} OAMD objects, {renderer.Objects.Count} renderer sources, " +
    $"{beds} beds (LFE object index {lfePosition}), bed channels {string.Join(' ', bedChannels.Select(Short))}.");

// Instrumentation state
long windowStart = (long)(opts.WindowStart * 48000), windowEnd = (long)(opts.WindowEnd * 48000);
bool instrumenting = opts.LogJoc || opts.LogObjects || opts.DumpObjects || opts.DumpBed;
FileStream? bedDump = opts.DumpBed ? new FileStream(Path.Combine(output, "bed-as-seen.f32le"), FileMode.CreateNew) : null;
var inputDataField = typeof(EnhancedAC3Renderer).GetField("inputData", any)!;
var timeslotPositionField = typeof(EnhancedAC3Renderer).GetField("timeslotPosition", any)!;
long bedDumpStart = -1, bedDumpTimeslots = 0;
StreamWriter? jocLog = opts.LogJoc ? new StreamWriter(Path.Combine(output, "joc-log.jsonl")) : null;
StreamWriter? objectLog = opts.LogObjects ? new StreamWriter(Path.Combine(output, "objects-log.jsonl")) : null;
FileStream? objectDump = opts.DumpObjects ? new FileStream(Path.Combine(output, "objects.f32le"), FileMode.CreateNew) : null;
long dumpStart = -1, dumpFrames = 0;
int objectCount = renderer.Objects.Count;
float[] dumpBuffer = new float[block * objectCount];
int lastFetch = int.MaxValue, lastApplierTimeslot = -1;
long frameIndex = -1, framesInWindow = 0, sparseFrames = 0, sparseObjectFrames = 0, misalignedFrames = 0;
HashSet<int> sparseObjects = new();
int firstSparseFrame = -1, lastSparseFrame = -1;
long nextProgress = 48000 * 20;
while (rendered < reader.Length) {
    float[] samples = listener.Render();
    int count = (int)Math.Min(block, reader.Length - rendered);
    if (renderer.HasObjects) objectBlocks++; else fallbackBlocks++;
    minJoc = Math.Min(minJoc, IntProperty(joc, "ObjectCount"));
    maxJoc = Math.Max(maxJoc, IntProperty(joc, "ObjectCount"));
    minOamd = Math.Min(minOamd, IntProperty(oamd, "ObjectCount"));
    maxOamd = Math.Max(maxOamd, IntProperty(oamd, "ObjectCount"));
    for (int sample = 0; sample < count; sample++) {
        float bass = 0;
        if (bassFilters.Length != 0) {
            float sum = 0;
            for (int channel = 0; channel < channels; channel++) if (!lfeChannel[channel]) sum += samples[sample * channels + permutation[channel]];
            float filtered = sum;
            foreach (Biquad filter in bassFilters) filtered = filter.Process(filtered);
            bass = opts.BassManageGain * filtered;
        }
        for (int channel = 0; channel < channels; channel++) {
            float value = samples[sample * channels + permutation[channel]];
            if (lfeChannel[channel]) {
                if (lfeDelayLines[channel] is float[] line) { float delayed = line[lfeDelayAt]; line[lfeDelayAt] = value; value = delayed; }
                if (opts.LfeGain != 1) value *= opts.LfeGain;
                value += bass;
            }
            reordered[sample * channels + channel] = value;
            if (!float.IsFinite(value)) { nonfinite++; continue; }
            double magnitude = Math.Abs((double)value);
            sums[channel] += (double)value * value;
            peaks[channel] = Math.Max(peaks[channel], magnitude);
            if (value != 0) nonzero[channel]++;
            if (magnitude > 1) aboveOne[channel]++;
        }
        if (opts.LfeDelay > 0 && ++lfeDelayAt == opts.LfeDelay) lfeDelayAt = 0;
    }
    pcm.Write(MemoryMarshal.AsBytes(reordered.AsSpan(0, count * channels)));

    // Frame / JOC bookkeeping: a drop of the decoder's fetch position means a new E-AC-3 frame was decoded
    // (and its EMDF payloads parsed) during this render call.
    int fetch = decoder.LastFetchStart;
    bool newFrame = fetch < lastFetch;
    lastFetch = fetch;
    int applierTimeslot = (int)applierTimeslotField.GetValue(applier)!;
    bool matricesComputed = applierTimeslot == 1 && lastApplierTimeslot != 1;
    lastApplierTimeslot = applierTimeslot;
    bool[] active = (bool[])JocField("ObjectActive");
    bool[] sparse = (bool[])JocField("sparseCoded");
    if (newFrame) {
        frameIndex++;
        if (applierTimeslot != 1) misalignedFrames++;
        int[] dpNow = (int[])JocField("dataPoints");
        bool[] steepNow = (bool[])JocField("steepSlope");
        bool anyTwo = false, anySteep = false;
        for (int obj = 0; obj < active.Length; obj++) {
            if (!active[obj]) continue;
            if (dpNow[obj] == 2) anyTwo = true;
            if (steepNow[obj]) anySteep = true;
        }
        if (anyTwo) framesWithTwoDataPoints++;
        if (anySteep) framesWithSteep++;
        bool anySparse = false;
        for (int obj = 0; obj < active.Length; obj++) {
            if (active[obj] && sparse[obj]) {
                anySparse = true;
                sparseObjectFrames++;
                sparseObjects.Add(obj);
            }
        }
        if (anySparse) {
            sparseFrames++;
            if (firstSparseFrame < 0) firstSparseFrame = (int)frameIndex;
            lastSparseFrame = (int)frameIndex;
        }
    }
    bool inWindow = rendered + count > windowStart && rendered < windowEnd;
    if (instrumenting && inWindow) {
        if (newFrame) framesInWindow++;
        if (jocLog != null && newFrame) {
            byte[] bandsIndex = (byte[])JocField("bandsIndex"), bands = (byte[])JocField("bands"), quant = (byte[])JocField("quantizationTable");
            int[] dataPoints = (int[])JocField("dataPoints");
            bool[] steep = (bool[])JocField("steepSlope");
            int[][] offsets = (int[][])JocField("timeslotOffsets");
            var objectsInfo = Enumerable.Range(0, active.Length).Select(obj => new {
                obj, active = active[obj], sparse = sparse[obj], bands = (int)bands[obj], bands_index = (int)bandsIndex[obj],
                quant = (int)quant[obj], data_points = dataPoints[obj], steep = steep[obj],
                offsets = steep[obj] ? offsets[obj].Take(dataPoints[obj]).ToArray() : null }).ToArray();
            jocLog.WriteLine(JsonSerializer.Serialize(new { kind = "frame", frame = frameIndex, out_sample = rendered,
                out_time = rendered / 48000.0, applier_timeslot_after = applierTimeslot,
                gain = Convert.ToSingle(joc.GetType().GetProperty("Gain")!.GetValue(joc)), objects = objectsInfo }));
        }
        if (jocLog != null && matricesComputed) {
            float[][][][] mix = (float[][][][])JocField("mixMatrix");
            byte[] bands = (byte[])JocField("bands");
            int[] dataPoints = (int[])JocField("dataPoints");
            var matrices = Enumerable.Range(0, active.Length).Where(obj => active[obj]).Select(obj => new { obj, sparse = sparse[obj],
                dp = Enumerable.Range(0, dataPoints[obj]).Select(dp => mix[obj][dp].Select(ch => ch.Take(bands[obj])
                    .Select(v => MathF.Round(v, 4)).ToArray()).ToArray()).ToArray() }).ToArray();
            jocLog.WriteLine(JsonSerializer.Serialize(new { kind = "matrices", frame = frameIndex, out_sample = rendered,
                out_time = rendered / 48000.0, objects = matrices }));
        }
        if (objectLog != null) {
            var state = renderer.Objects.Select((source, index) => new { obj = index,
                pos = new[] { MathF.Round(source.Position.X, 3), MathF.Round(source.Position.Y, 3), MathF.Round(source.Position.Z, 3) },
                vol = MathF.Round(source.Volume, 4), size = MathF.Round(source.Size, 3), lfe = source.LFE }).ToArray();
            objectLog.WriteLine(JsonSerializer.Serialize(new { out_sample = rendered, out_time = rendered / 48000.0, frame = frameIndex, objects = state }));
        }
        if (bedDump != null) {
            // A timeslot was fetched during this render call when the renderer's timeslot cursor is mid-slot or just wrapped.
            int timeslotPosition = (int)timeslotPositionField.GetValue(renderer)!;
            if (block >= 64 || timeslotPosition == block % 64) {
                float[][] inputData = (float[][])inputDataField.GetValue(renderer)!;
                if (bedDumpStart < 0) bedDumpStart = rendered;
                int bedChannelCount = inputData.Length;
                float[] slot = new float[64 * bedChannelCount];
                for (int sample = 0; sample < 64; sample++)
                    for (int channel = 0; channel < bedChannelCount; channel++)
                        slot[sample * bedChannelCount + channel] = inputData[channel][sample];
                bedDump.Write(MemoryMarshal.AsBytes(slot.AsSpan()));
                bedDumpTimeslots++;
            }
        }
        if (objectDump != null) {
            float[][] objectSamples = (float[][])objectSamplesField.GetValue(renderer)!;
            if (dumpStart < 0) dumpStart = rendered;
            for (int sample = 0; sample < count; sample++)
                for (int obj = 0; obj < objectCount; obj++)
                    dumpBuffer[sample * objectCount + obj] = objectSamples[obj][sample];
            objectDump.Write(MemoryMarshal.AsBytes(dumpBuffer.AsSpan(0, count * objectCount)));
            dumpFrames += count;
        }
    }

    rendered += count;
    if (rendered >= nextProgress) {
        pcm.Flush();
        Console.WriteLine($"Rendered {rendered / 48000.0:F1}s; fallback blocks: {fallbackBlocks}");
        nextProgress += 48000 * 20;
    }
}
pcm.Flush(true);
jocLog?.Dispose();
objectLog?.Dispose();
objectDump?.Dispose();
bedDump?.Dispose();
if (bedDump != null) {
    File.WriteAllText(Path.Combine(output, "bed-as-seen.json"), JsonSerializer.Serialize(new { sample_rate = 48000,
        channels = ((float[][])inputDataField.GetValue(renderer)!).Length, format = "f32le interleaved, decoder channel order as fed to the JOC applier (64-sample timeslots)",
        first_output_sample = bedDumpStart, timeslots = bedDumpTimeslots, note = "only valid for --update-rate 32 or multiples of 64" }, new JsonSerializerOptions { WriteIndented = true }));
}
watch.Stop();
if (objectDump != null) {
    File.WriteAllText(Path.Combine(output, "objects-dump.json"), JsonSerializer.Serialize(new { sample_rate = 48000, channels = objectCount,
        format = "f32le interleaved, one channel per renderer object (raw JOC output before volume and spatialisation)",
        start_sample = dumpStart, frames = dumpFrames, beds, lfe_object = lfePosition }, new JsonSerializerOptions { WriteIndented = true }));
}
var stats = names.Select((name, index) => new { channel = index + 1, name,
    peak = peaks[index], rms = Math.Sqrt(sums[index] / rendered),
    nonzero_samples = nonzero[index], samples_above_one = aboveOne[index] }).ToArray();
var summary = new { sample_rate = 48000, channels, format = "f32le interleaved",
    frames = rendered, duration_seconds = rendered / 48000.0, bytes = rendered * channels * 4,
    renderer = renderer.GetType().Name, joc_object_count_min = minJoc, joc_object_count_max = maxJoc,
    oamd_object_count_min = minOamd, oamd_object_count_max = maxOamd,
    rendered_object_blocks = objectBlocks, channel_fallback_blocks = fallbackBlocks,
    observation_block_frames = block, nonfinite_samples = nonfinite,
    elapsed_seconds = watch.Elapsed.TotalSeconds, realtime_factor = rendered / 48000.0 / watch.Elapsed.TotalSeconds,
    permutation, layout = opts.Layout == "ref916" ? "Cavern ref916 reordered to the existing Roc PCM route" : $"Cavern {opts.Layout} in Cavern order",
    headphone_virtualizer = false, normalizer = opts.Normalizer != 0, upmix = false,
    cavern_source_gain = 0.707, speaker_geometry_matched_to_apple = false, channels_stats = stats,
    lab = new { mode = opts.Mode, update_rate = block, layout = opts.Layout, channel_names = names,
        lfe_separation = opts.LfeSeparation, direct_lfe = opts.DirectLfe, lfe_gain = opts.LfeGain, lfe_delay = opts.LfeDelay, normalizer = opts.Normalizer,
        joc_timeslot_fix = opts.JocTimeslotFix, mute_sparse = opts.MuteSparse, sparse_gain = opts.SparseGain,
        window_seconds = new[] { opts.WindowStart, opts.WindowEnd }, frames_decoded = frameIndex + 1,
        frames_with_sparse_objects = sparseFrames, sparse_object_frames = sparseObjectFrames,
        sparse_objects = sparseObjects.OrderBy(x => x).ToArray(), first_sparse_frame = firstSparseFrame, last_sparse_frame = lastSparseFrame,
        first_sparse_time = firstSparseFrame < 0 ? -1 : firstSparseFrame * 1536 / 48000.0,
        last_sparse_time = lastSparseFrame < 0 ? -1 : (lastSparseFrame + 1) * 1536 / 48000.0,
        frames_where_applier_timeslot_misaligned = misalignedFrames, beds, lfe_object = lfePosition,
        joc_dmx_config_idx = downmixConfig, joc_channels = IntProperty(joc, "ChannelCount"),
        frames_with_two_data_points = framesWithTwoDataPoints, frames_with_steep_slope = framesWithSteep,
        surround_phase = opts.SurroundPhase, joc_interp_fix = opts.JocInterpFix,
        bass_manage_gain = opts.BassManageGain, bass_manage_freq = opts.BassManageFreq, bass_type = opts.BassType,
        bed_channels = bedChannels.Select(Short).ToArray() } };
File.WriteAllText(Path.Combine(output, "render-summary.json"), JsonSerializer.Serialize(summary, new JsonSerializerOptions { WriteIndented = true }));
Console.WriteLine($"Finished: {rendered / 48000.0:F3}s rendered in {watch.Elapsed.TotalSeconds:F2}s; frames {frameIndex + 1}, dmx config {downmixConfig}, " +
    $"frames with 2 data points {framesWithTwoDataPoints}, with steep slope {framesWithSteep}, " +
    $"frames with sparse objects {sparseFrames} (objects {string.Join(',', sparseObjects.OrderBy(x => x))}, " +
    $"{(firstSparseFrame < 0 ? "none" : $"{firstSparseFrame * 1536 / 48000.0:F2}s-{(lastSparseFrame + 1) * 1536 / 48000.0:F2}s")}), " +
    $"applier timeslot misaligned at {misalignedFrames} frame starts.");
if (fallbackBlocks != 0 || nonfinite != 0 || nonzero.All(value => value == 0))
    throw new Exception("Render validation failed; inspect render-summary.json");

static string Short(ReferenceChannel channel) => channel switch {
    ReferenceChannel.FrontLeft => "FL", ReferenceChannel.FrontRight => "FR", ReferenceChannel.FrontCenter => "FC",
    ReferenceChannel.ScreenLFE => "LFE", ReferenceChannel.RearLeft => "RL", ReferenceChannel.RearRight => "RR",
    ReferenceChannel.SideLeft => "SL", ReferenceChannel.SideRight => "SR", ReferenceChannel.WideLeft => "FLW",
    ReferenceChannel.WideRight => "FRW", ReferenceChannel.TopFrontLeft => "TFL", ReferenceChannel.TopFrontRight => "TFR",
    ReferenceChannel.TopSideLeft => "TSL", ReferenceChannel.TopSideRight => "TSR", ReferenceChannel.TopRearLeft => "TRL",
    ReferenceChannel.TopRearRight => "TRR", _ => channel.ToString() };

static void RunBedDecode(string input, string output, LabOptions opts) {
    string[] labels;
    using (AudioReader probeReader = AudioReader.Open(input)) {
        probeReader.ReadHeader();
        using Renderer probeRenderer = probeReader.GetRenderer();
        // The decoder interlaces its channels in ReferenceChannel enum order.
        labels = probeRenderer.GetChannels().OrderBy(c => c).Select(Short).ToArray();
    }
    using AudioReader reader = AudioReader.Open(input);
    reader.ReadHeader();
    // The MP4 sample entry of an Atmos track declares 2 channels; the decoder's own channel list is authoritative.
    int channels = labels.Length;
    long length = reader.Length;
    Console.WriteLine($"Container declares {reader.ChannelCount} channels, decoder yields {channels}: {string.Join(' ', labels)}");
    string rawPath = Path.Combine(output, "bed.f32le");
    using FileStream pcm = new(rawPath, FileMode.CreateNew, FileAccess.Write, FileShare.Read, 1024 * 1024);
    const int chunk = 4096;
    float[] buffer = new float[chunk * channels];
    double[] sums = new double[channels], peaks = new double[channels];
    long[] nonzero = new long[channels];
    long done = 0, nonfinite = 0;
    Stopwatch watch = Stopwatch.StartNew();
    while (done < length) {
        int count = (int)Math.Min(chunk, length - done);
        reader.ReadBlock(buffer, 0, (long)count * channels);
        for (int sample = 0; sample < count; sample++) {
            for (int channel = 0; channel < channels; channel++) {
                float value = buffer[sample * channels + channel];
                if (!float.IsFinite(value)) { nonfinite++; continue; }
                sums[channel] += (double)value * value;
                peaks[channel] = Math.Max(peaks[channel], Math.Abs((double)value));
                if (value != 0) nonzero[channel]++;
            }
        }
        pcm.Write(MemoryMarshal.AsBytes(buffer.AsSpan(0, count * channels)));
        done += count;
    }
    pcm.Flush(true);
    var stats = labels.Select((name, index) => new { channel = index + 1, name, peak = peaks[index],
        rms = Math.Sqrt(sums[index] / done), nonzero_samples = nonzero[index] }).ToArray();
    var summary = new { mode = "bed", sample_rate = reader.SampleRate, channels, channel_names = labels, format = "f32le interleaved",
        frames = done, duration_seconds = done / (double)reader.SampleRate, nonfinite_samples = nonfinite,
        elapsed_seconds = watch.Elapsed.TotalSeconds, note = "E-AC-3 core (JOC downmix) decoded by Cavern's EnhancedAC3Decoder; no JOC, no renderer",
        channels_stats = stats };
    File.WriteAllText(Path.Combine(output, "bed-summary.json"), JsonSerializer.Serialize(summary, new JsonSerializerOptions { WriteIndented = true }));
    Console.WriteLine($"Bed decode finished: {done} frames, {channels} channels ({string.Join(' ', labels)}) in {watch.Elapsed.TotalSeconds:F2}s.");
}

static void RunQmfTest(string output) {
    // Round-trip test of Cavern's QMF bank: white noise, a 1 kHz tone and a 10 kHz tone through analysis + synthesis
    // with a unity matrix. Reports the bank delay and the reconstruction SNR after delay compensation.
    const int length = 48000 * 2;
    Random random = new(1234);
    float[] noise = new float[length], tone1k = new float[length], tone10k = new float[length], sweep = new float[length];
    for (int i = 0; i < length; i++) {
        noise[i] = (float)(random.NextDouble() * 2 - 1) * .5f;
        tone1k[i] = .5f * MathF.Sin(2 * MathF.PI * 1000 * i / 48000f);
        tone10k[i] = .5f * MathF.Sin(2 * MathF.PI * 10000 * i / 48000f);
        double f = 20 * Math.Pow(1000, i / (double)length); // 20 Hz .. 20 kHz
        sweep[i] = .5f * (float)Math.Sin(2 * Math.PI * 20 * length / Math.Log(1000) * (Math.Pow(1000, i / (double)length) - 1) / 48000);
    }
    var report = new List<object>();
    foreach ((string name, float[] signal) in new[] { ("noise", noise), ("tone1k", tone1k), ("tone10k", tone10k), ("sweep", sweep) }) {
        float[] result = LabQmfTest.RoundTrip(signal);
        int bestDelay = 0; double bestError = double.MaxValue, bestGain = 1;
        for (int delay = 0; delay < 2048; delay++) {
            double num = 0, den = 0;
            for (int i = delay + 4096; i < length - 4096; i++) { num += (double)result[i] * signal[i - delay]; den += (double)signal[i - delay] * signal[i - delay]; }
            double gain = den > 0 ? num / den : 0, error = 0;
            for (int i = delay + 4096; i < length - 4096; i++) { double e = result[i] - gain * signal[i - delay]; error += e * e; }
            if (error < bestError) { bestError = error; bestDelay = delay; bestGain = gain; }
        }
        double signalEnergy = 0;
        for (int i = bestDelay + 4096; i < length - 4096; i++) signalEnergy += (double)signal[i - bestDelay] * signal[i - bestDelay];
        double snr = 10 * Math.Log10(signalEnergy * bestGain * bestGain / Math.Max(bestError, 1e-30));
        report.Add(new { signal = name, delay_samples = bestDelay, gain = bestGain, gain_db = 20 * Math.Log10(Math.Abs(bestGain)), snr_db = snr });
        Console.WriteLine($"QMF round trip {name}: delay {bestDelay} samples, gain {20 * Math.Log10(Math.Abs(bestGain)):F2} dB, SNR {snr:F1} dB");
        File.WriteAllBytes(Path.Combine(output, $"qmf-{name}-out.f32le"), MemoryMarshal.AsBytes(result.AsSpan()).ToArray());
        File.WriteAllBytes(Path.Combine(output, $"qmf-{name}-in.f32le"), MemoryMarshal.AsBytes(signal.AsSpan()).ToArray());
    }
    File.WriteAllText(Path.Combine(output, "qmf-test.json"), JsonSerializer.Serialize(report, new JsonSerializerOptions { WriteIndented = true }));
}

/// <summary>RBJ cookbook low-pass biquad (direct form I) at 48 kHz.</summary>
class Biquad {
    readonly float b0, b1, b2, a1, a2;
    float x1, x2, y1, y2;
    public Biquad(float f0, float q) {
        float w0 = 2 * MathF.PI * f0 / 48000, alpha = MathF.Sin(w0) / (2 * q), cos = MathF.Cos(w0), a0 = 1 + alpha;
        b0 = (1 - cos) / 2 / a0; b1 = (1 - cos) / a0; b2 = b0; a1 = -2 * cos / a0; a2 = (1 - alpha) / a0;
    }
    Biquad(float b0, float b1, float a1) { this.b0 = b0; this.b1 = b1; this.a1 = a1; }
    /// <summary>First-order low-pass (bilinear transform of 1/(1+s/w0)).</summary>
    public static Biquad FirstOrder(float f0) {
        float k = MathF.Tan(MathF.PI * f0 / 48000), norm = 1 / (1 + k);
        return new Biquad(k * norm, k * norm, (k - 1) * norm);
    }
    public float Process(float x) {
        float y = b0 * x + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
        x2 = x1; x1 = x; y2 = y1; y1 = y;
        return y;
    }
}

class LabOptions {
    public string Mode = "render";
    public int UpdateRate = 32;
    public string Layout = "ref916";
    public bool LfeSeparation = true, DirectLfe = true;
    public float Normalizer = 0, LfeGain = 1, SparseGain = 1;
    public int LfeDelay;
    public double WindowStart = 4.5, WindowEnd = 7.9;
    public bool LogJoc, LogObjects, DumpObjects, DumpBed, JocTimeslotFix, MuteSparse, JocInterpFix;
    public int SurroundPhase;
    public float BassManageGain, BassManageFreq = 90;
    public string BassType = "bw4";
}
