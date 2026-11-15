using System;

namespace Cavern.Format.Decoders.EnhancedAC3 {
    /// <summary>
    /// am-ui cavern-lab: exposes a QMF analysis/synthesis round trip (identity mixing, one object) for testing.
    /// </summary>
    public static class LabQmfTest {
        /// <summary>
        /// Passes <paramref name="input"/> (any length, multiple of 64) through ProcessForward and ProcessInverse
        /// with a unity mixing matrix and returns the synthesized signal (same length, includes the bank's delay).
        /// </summary>
        public static unsafe float[] RoundTrip(float[] input) {
            QuadratureMirrorFilterBank analysis = new QuadratureMirrorFilterBank(), synthesis = new QuadratureMirrorFilterBank();
            float[] output = new float[input.Length];
            float[] timeslotOut = new float[QuadratureMirrorFilterBank.subbands];
            (float[] real, float[] imaginary) copy = (new float[QuadratureMirrorFilterBank.subbands], new float[QuadratureMirrorFilterBank.subbands]);
            for (int pos = 0; pos + QuadratureMirrorFilterBank.subbands <= input.Length; pos += QuadratureMirrorFilterBank.subbands) {
                fixed (float* pInput = &input[pos]) {
                    (float[] real, float[] imaginary) = analysis.ProcessForward(pInput);
                    Array.Copy(real, copy.real, real.Length);
                    Array.Copy(imaginary, copy.imaginary, imaginary.Length);
                }
                synthesis.ProcessInverse(copy, timeslotOut);
                Array.Copy(timeslotOut, 0, output, pos, QuadratureMirrorFilterBank.subbands);
            }
            return output;
        }
    }
}
