namespace Cavern.Format.Decoders.EnhancedAC3 {
    /// <summary>
    /// am-ui cavern-lab experiment switches. Every default reproduces the upstream behaviour bit for bit.
    /// </summary>
    public static class LabSwitches {
        /// <summary>
        /// Wrap <see cref="JointObjectCodingApplier"/>'s timeslot counter at the number of QMF timeslots in a frame
        /// (frame length / 64) instead of the number of input channels (7), so the interpolated JOC matrices are
        /// recomputed once per frame and every interpolated timeslot is used.
        /// </summary>
        public static bool JocTimeslotWrapAtFrame;

        /// <summary>
        /// Zero the mixing matrices of sparse-coded objects (the behaviour before upstream #419).
        /// </summary>
        public static bool MuteSparseObjects;

        /// <summary>
        /// Scale applied to the dequantization step of sparse-coded objects (1 = upstream).
        /// </summary>
        public static float SparseGainScale = 1;

        /// <summary>
        /// Multiply the QMF samples of the Ls/Rs downmix channels by +j (1) or -j (-1) before JOC reconstruction
        /// when joc_dmx_config_idx is 3 or 4 ("with 90 degree phase shift"; upstream issue #423). -1 is the value
        /// that matches Apple's renderer (am-ui research/cavern-artifact.md).
        /// </summary>
        public static int SurroundPhaseShift;

        /// <summary>
        /// Interpolate the JOC matrices exactly as ETSI TS 103 420 clause 6.6.5 (Pseudocode 6) describes, with the
        /// parameter band to subband mapping applied in every branch.
        /// </summary>
        public static bool JocInterpolationFix;
    }
}
