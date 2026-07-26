//! Tag names, grouped by the directory they appear in.
//!
//! Tag numbers are only unique *within* a directory: tag 1 is `GPSLatitudeRef` under
//! GPS and `InteroperabilityIndex` under Interop. Looking a number up without knowing
//! where it came from is how metadata tools produce confident nonsense.
//!
//! Coverage is TIFF baseline + Exif + GPS + DNG/TIFF-EP. The long tail of vendor tags
//! is deliberately absent — see the crate docs.

use blad_container::ifd::IfdKind;

/// Semantic hint used for formatting. The dictionary says what a tag *is*; the
/// formatter decides how to show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Plain,
    /// Shutter speed — render as a fraction with a unit.
    Seconds,
    FNumber,
    Millimetres,
    Iso,
    /// An enumerated value with a lookup table.
    Enum,
    /// A 3x3 colour matrix, worth showing as a matrix.
    Matrix3x3,
    /// Personally identifying — suppressed by `--redact`.
    Sensitive,
    /// A pointer to another directory; the directory itself is shown instead.
    Pointer,
    /// Opaque vendor data we decline to interpret.
    Opaque,
    /// An Exif timestamp, normalised to ISO-8601.
    DateTime,
}

pub struct Tag {
    pub name: &'static str,
    pub kind: Kind,
}

const fn t(name: &'static str) -> Tag {
    Tag {
        name,
        kind: Kind::Plain,
    }
}
const fn k(name: &'static str, kind: Kind) -> Tag {
    Tag { name, kind }
}

/// Look up a tag by number and directory.
pub fn lookup(ifd: IfdKind, tag: u16) -> Option<Tag> {
    match ifd {
        IfdKind::Gps => gps(tag),
        IfdKind::Interop => interop(tag),
        // Exif, main and sub directories share the TIFF/Exif/DNG number space.
        _ => tiff(tag),
    }
}

fn interop(tag: u16) -> Option<Tag> {
    Some(match tag {
        1 => t("InteroperabilityIndex"),
        2 => t("InteroperabilityVersion"),
        4096 => t("RelatedImageFileFormat"),
        4097 => t("RelatedImageWidth"),
        4098 => t("RelatedImageLength"),
        _ => return None,
    })
}

fn gps(tag: u16) -> Option<Tag> {
    Some(match tag {
        0 => t("GPSVersionID"),
        1 => k("GPSLatitudeRef", Kind::Sensitive),
        2 => k("GPSLatitude", Kind::Sensitive),
        3 => k("GPSLongitudeRef", Kind::Sensitive),
        4 => k("GPSLongitude", Kind::Sensitive),
        5 => k("GPSAltitudeRef", Kind::Sensitive),
        6 => k("GPSAltitude", Kind::Sensitive),
        7 => t("GPSTimeStamp"),
        8 => t("GPSSatellites"),
        9 => t("GPSStatus"),
        10 => t("GPSMeasureMode"),
        11 => t("GPSDOP"),
        12 => t("GPSSpeedRef"),
        13 => t("GPSSpeed"),
        14 => t("GPSTrackRef"),
        15 => t("GPSTrack"),
        16 => t("GPSImgDirectionRef"),
        17 => t("GPSImgDirection"),
        18 => t("GPSMapDatum"),
        19 => k("GPSDestLatitudeRef", Kind::Sensitive),
        20 => k("GPSDestLatitude", Kind::Sensitive),
        21 => k("GPSDestLongitudeRef", Kind::Sensitive),
        22 => k("GPSDestLongitude", Kind::Sensitive),
        23 => t("GPSDestBearingRef"),
        24 => t("GPSDestBearing"),
        25 => t("GPSDestDistanceRef"),
        26 => t("GPSDestDistance"),
        27 => t("GPSProcessingMethod"),
        28 => t("GPSAreaInformation"),
        29 => t("GPSDateStamp"),
        30 => t("GPSDifferential"),
        31 => t("GPSHPositioningError"),
        _ => return None,
    })
}

fn tiff(tag: u16) -> Option<Tag> {
    Some(match tag {
        // --- TIFF baseline ---
        254 => k("NewSubfileType", Kind::Enum),
        255 => k("SubfileType", Kind::Enum),
        256 => t("ImageWidth"),
        257 => t("ImageLength"),
        258 => t("BitsPerSample"),
        259 => k("Compression", Kind::Enum),
        262 => k("PhotometricInterpretation", Kind::Enum),
        263 => t("Thresholding"),
        266 => t("FillOrder"),
        269 => t("DocumentName"),
        270 => t("ImageDescription"),
        271 => t("Make"),
        272 => t("Model"),
        273 => t("StripOffsets"),
        274 => k("Orientation", Kind::Enum),
        277 => t("SamplesPerPixel"),
        278 => t("RowsPerStrip"),
        279 => t("StripByteCounts"),
        282 => t("XResolution"),
        283 => t("YResolution"),
        284 => k("PlanarConfiguration", Kind::Enum),
        296 => k("ResolutionUnit", Kind::Enum),
        297 => t("PageNumber"),
        301 => t("TransferFunction"),
        305 => t("Software"),
        306 => k("DateTime", Kind::DateTime),
        315 => k("Artist", Kind::Sensitive),
        316 => t("HostComputer"),
        317 => t("Predictor"),
        318 => t("WhitePoint"),
        319 => t("PrimaryChromaticities"),
        320 => t("ColorMap"),
        322 => t("TileWidth"),
        323 => t("TileLength"),
        324 => t("TileOffsets"),
        325 => t("TileByteCounts"),
        330 => k("SubIFDs", Kind::Pointer),
        338 => t("ExtraSamples"),
        339 => t("SampleFormat"),
        347 => t("JPEGTables"),
        512 => t("JPEGProc"),
        513 => t("JPEGInterchangeFormat"),
        514 => t("JPEGInterchangeFormatLength"),
        529 => t("YCbCrCoefficients"),
        530 => t("YCbCrSubSampling"),
        531 => k("YCbCrPositioning", Kind::Enum),
        532 => t("ReferenceBlackWhite"),
        33421 => t("CFARepeatPatternDim"),
        33422 => t("CFAPattern"),
        33423 => t("BatteryLevel"),
        33432 => k("Copyright", Kind::Sensitive),
        33434 => k("ExposureTime", Kind::Seconds),
        33437 => k("FNumber", Kind::FNumber),
        34665 => k("ExifIFD", Kind::Pointer),
        34675 => k("InterColorProfile", Kind::Opaque),
        34853 => k("GPSIFD", Kind::Pointer),

        // --- Exif ---
        34850 => k("ExposureProgram", Kind::Enum),
        34852 => t("SpectralSensitivity"),
        34855 => k("ISOSpeedRatings", Kind::Iso),
        34856 => t("OECF"),
        34864 => k("SensitivityType", Kind::Enum),
        34865 => t("StandardOutputSensitivity"),
        34866 => t("RecommendedExposureIndex"),
        34867 => k("ISOSpeed", Kind::Iso),
        36864 => t("ExifVersion"),
        36867 => k("DateTimeOriginal", Kind::DateTime),
        36868 => k("DateTimeDigitized", Kind::DateTime),
        36880 => t("OffsetTime"),
        36881 => t("OffsetTimeOriginal"),
        36882 => t("OffsetTimeDigitized"),
        37121 => t("ComponentsConfiguration"),
        37122 => t("CompressedBitsPerPixel"),
        37377 => t("ShutterSpeedValue"),
        37378 => t("ApertureValue"),
        37379 => t("BrightnessValue"),
        37380 => t("ExposureBiasValue"),
        37381 => t("MaxApertureValue"),
        37382 => t("SubjectDistance"),
        37383 => k("MeteringMode", Kind::Enum),
        37384 => k("LightSource", Kind::Enum),
        37385 => k("Flash", Kind::Enum),
        37386 => k("FocalLength", Kind::Millimetres),
        37396 => t("SubjectArea"),
        37500 => k("MakerNote", Kind::Opaque),
        37510 => t("UserComment"),
        37520 => t("SubSecTime"),
        37521 => t("SubSecTimeOriginal"),
        37522 => t("SubSecTimeDigitized"),
        40960 => t("FlashpixVersion"),
        40961 => k("ColorSpace", Kind::Enum),
        40962 => t("PixelXDimension"),
        40963 => t("PixelYDimension"),
        40964 => t("RelatedSoundFile"),
        40965 => k("InteropIFD", Kind::Pointer),
        41483 => t("FlashEnergy"),
        41486 => t("FocalPlaneXResolution"),
        41487 => t("FocalPlaneYResolution"),
        41488 => k("FocalPlaneResolutionUnit", Kind::Enum),
        41492 => t("SubjectLocation"),
        41493 => t("ExposureIndex"),
        41495 => k("SensingMethod", Kind::Enum),
        41728 => t("FileSource"),
        41729 => t("SceneType"),
        41730 => t("CFAPattern"),
        41985 => k("CustomRendered", Kind::Enum),
        41986 => k("ExposureMode", Kind::Enum),
        41987 => k("WhiteBalance", Kind::Enum),
        41988 => t("DigitalZoomRatio"),
        41989 => k("FocalLengthIn35mmFilm", Kind::Millimetres),
        41990 => k("SceneCaptureType", Kind::Enum),
        41991 => t("GainControl"),
        41992 => t("Contrast"),
        41993 => t("Saturation"),
        41994 => t("Sharpness"),
        41996 => t("SubjectDistanceRange"),
        42016 => k("ImageUniqueID", Kind::Sensitive),
        42032 => k("CameraOwnerName", Kind::Sensitive),
        42033 => k("BodySerialNumber", Kind::Sensitive),
        42034 => t("LensSpecification"),
        42035 => t("LensMake"),
        42036 => t("LensModel"),
        42037 => k("LensSerialNumber", Kind::Sensitive),
        42080 => t("CompositeImage"),

        // --- DNG / TIFF-EP: camera characterization ---
        50706 => t("DNGVersion"),
        50707 => t("DNGBackwardVersion"),
        50708 => t("UniqueCameraModel"),
        50709 => t("LocalizedCameraModel"),
        50710 => t("CFAPlaneColor"),
        50711 => k("CFALayout", Kind::Enum),
        50712 => t("LinearizationTable"),
        50713 => t("BlackLevelRepeatDim"),
        50714 => t("BlackLevel"),
        50715 => t("BlackLevelDeltaH"),
        50716 => t("BlackLevelDeltaV"),
        50717 => t("WhiteLevel"),
        50718 => t("DefaultScale"),
        50719 => t("DefaultCropOrigin"),
        50720 => t("DefaultCropSize"),
        50721 => k("ColorMatrix1", Kind::Matrix3x3),
        50722 => k("ColorMatrix2", Kind::Matrix3x3),
        50723 => k("CameraCalibration1", Kind::Matrix3x3),
        50724 => k("CameraCalibration2", Kind::Matrix3x3),
        50725 => k("ReductionMatrix1", Kind::Matrix3x3),
        50726 => k("ReductionMatrix2", Kind::Matrix3x3),
        50727 => t("AnalogBalance"),
        50728 => t("AsShotNeutral"),
        50729 => t("AsShotWhiteXY"),
        50730 => t("BaselineExposure"),
        50731 => t("BaselineNoise"),
        50732 => t("BaselineSharpness"),
        50733 => t("BayerGreenSplit"),
        50734 => t("LinearResponseLimit"),
        50735 => k("CameraSerialNumber", Kind::Sensitive),
        50736 => t("LensInfo"),
        50737 => t("ChromaBlurRadius"),
        50738 => t("AntiAliasStrength"),
        50739 => t("ShadowScale"),
        50740 => k("DNGPrivateData", Kind::Opaque),
        50741 => k("MakerNoteSafety", Kind::Enum),
        50778 => k("CalibrationIlluminant1", Kind::Enum),
        50779 => k("CalibrationIlluminant2", Kind::Enum),
        50780 => t("BestQualityScale"),
        50781 => t("RawDataUniqueID"),
        50827 => t("OriginalRawFileName"),
        50829 => t("ActiveArea"),
        50830 => t("MaskedAreas"),
        50831 => k("AsShotICCProfile", Kind::Opaque),
        50832 => t("AsShotPreProfileMatrix"),
        50833 => k("CurrentICCProfile", Kind::Opaque),
        50834 => t("CurrentPreProfileMatrix"),
        50879 => k("ColorimetricReference", Kind::Enum),
        50931 => t("CameraCalibrationSignature"),
        50932 => t("ProfileCalibrationSignature"),
        50936 => t("ProfileName"),
        50937 => t("ProfileHueSatMapDims"),
        50938 => k("ProfileHueSatMapData1", Kind::Opaque),
        50939 => k("ProfileHueSatMapData2", Kind::Opaque),
        50940 => k("ProfileToneCurve", Kind::Opaque),
        50941 => k("ProfileEmbedPolicy", Kind::Enum),
        50942 => t("ProfileCopyright"),
        50964 => k("ForwardMatrix1", Kind::Matrix3x3),
        50965 => k("ForwardMatrix2", Kind::Matrix3x3),
        50966 => t("PreviewApplicationName"),
        50967 => t("PreviewApplicationVersion"),
        50969 => t("PreviewSettingsDigest"),
        50970 => k("PreviewColorSpace", Kind::Enum),
        50971 => k("PreviewDateTime", Kind::DateTime),
        50972 => t("RawImageDigest"),
        51041 => t("NoiseProfile"),
        51043 => t("TimeCodes"),
        51044 => t("FrameRate"),
        51125 => t("DefaultUserCrop"),

        // --- Adobe / XMP ---
        700 => k("XMP", Kind::Opaque),
        33723 => k("IPTC", Kind::Opaque),
        _ => return None,
    })
}

/// Human text for enumerated values. Returns `None` when the number is not one we know,
/// in which case the caller shows the number — never a guess.
pub fn enum_text(name: &str, v: u64) -> Option<&'static str> {
    Some(match (name, v) {
        ("Compression", 1) => "uncompressed",
        ("Compression", 5) => "LZW",
        ("Compression", 6) => "JPEG (old)",
        ("Compression", 7) => "JPEG",
        ("Compression", 8) => "deflate",
        ("Compression", 32773) => "PackBits",
        ("Compression", 34892) => "lossy JPEG",

        ("PhotometricInterpretation", 0) => "white is zero",
        ("PhotometricInterpretation", 1) => "black is zero",
        ("PhotometricInterpretation", 2) => "RGB",
        ("PhotometricInterpretation", 3) => "palette",
        ("PhotometricInterpretation", 6) => "YCbCr",
        ("PhotometricInterpretation", 32803) => "CFA (Bayer mosaic)",
        ("PhotometricInterpretation", 34892) => "linear raw",

        ("Orientation", 1) => "upright",
        ("Orientation", 2) => "mirrored",
        ("Orientation", 3) => "rotated 180°",
        ("Orientation", 4) => "mirrored, rotated 180°",
        ("Orientation", 5) => "mirrored, rotated 90° CW",
        ("Orientation", 6) => "rotated 90° CW",
        ("Orientation", 7) => "mirrored, rotated 270° CW",
        ("Orientation", 8) => "rotated 270° CW",

        ("ResolutionUnit", 1) => "none",
        ("ResolutionUnit", 2) => "inch",
        ("ResolutionUnit", 3) => "cm",

        ("PlanarConfiguration", 1) => "chunky",
        ("PlanarConfiguration", 2) => "planar",

        ("NewSubfileType", 0) => "full-resolution image",
        ("NewSubfileType", 1) => "reduced-resolution image",
        ("NewSubfileType", 2) => "single page",

        ("ExposureProgram", 0) => "not defined",
        ("ExposureProgram", 1) => "manual",
        ("ExposureProgram", 2) => "normal",
        ("ExposureProgram", 3) => "aperture priority",
        ("ExposureProgram", 4) => "shutter priority",
        ("ExposureProgram", 5) => "creative",
        ("ExposureProgram", 6) => "action",
        ("ExposureProgram", 7) => "portrait",
        ("ExposureProgram", 8) => "landscape",

        ("MeteringMode", 0) => "unknown",
        ("MeteringMode", 1) => "average",
        ("MeteringMode", 2) => "centre-weighted",
        ("MeteringMode", 3) => "spot",
        ("MeteringMode", 4) => "multi-spot",
        ("MeteringMode", 5) => "pattern",
        ("MeteringMode", 6) => "partial",

        ("LightSource", 0) => "unknown",
        ("LightSource", 1) => "daylight",
        ("LightSource", 2) => "fluorescent",
        ("LightSource", 3) => "tungsten",
        ("LightSource", 4) => "flash",
        ("LightSource", 9) => "fine weather",
        ("LightSource", 10) => "cloudy",
        ("LightSource", 11) => "shade",

        ("Flash", 0) => "did not fire",
        ("Flash", 1) => "fired",
        ("Flash", 16) => "off, did not fire",
        ("Flash", 24) => "auto, did not fire",
        ("Flash", 25) => "auto, fired",

        ("ColorSpace", 1) => "sRGB",
        ("ColorSpace", 2) => "Adobe RGB",
        ("ColorSpace", 65535) => "uncalibrated",

        ("WhiteBalance", 0) => "auto",
        ("WhiteBalance", 1) => "manual",

        ("ExposureMode", 0) => "auto",
        ("ExposureMode", 1) => "manual",
        ("ExposureMode", 2) => "auto bracket",

        ("CustomRendered", 0) => "normal",
        ("CustomRendered", 1) => "custom",

        ("SceneCaptureType", 0) => "standard",
        ("SceneCaptureType", 1) => "landscape",
        ("SceneCaptureType", 2) => "portrait",
        ("SceneCaptureType", 3) => "night",

        ("SensingMethod", 1) => "not defined",
        ("SensingMethod", 2) => "one-chip colour area",
        ("SensingMethod", 7) => "trilinear",

        ("FocalPlaneResolutionUnit", 1) => "none",
        ("FocalPlaneResolutionUnit", 2) => "inch",
        ("FocalPlaneResolutionUnit", 3) => "cm",

        ("YCbCrPositioning", 1) => "centered",
        ("YCbCrPositioning", 2) => "co-sited",

        ("CFALayout", 1) => "rectangular",

        ("MakerNoteSafety", 0) => "unsafe",
        ("MakerNoteSafety", 1) => "safe",

        ("PreviewColorSpace", 1) => "gray gamma 2.2",
        ("PreviewColorSpace", 2) => "sRGB",
        ("PreviewColorSpace", 3) => "Adobe RGB",
        ("PreviewColorSpace", 4) => "ProPhoto RGB",

        ("ColorimetricReference", 0) => "scene-referred",
        ("ColorimetricReference", 1) => "output-referred",

        ("CalibrationIlluminant1", v) | ("CalibrationIlluminant2", v) => match v {
            0 => "unknown",
            1 => "daylight",
            2 => "fluorescent",
            3 => "tungsten",
            17 => "Standard A",
            18 => "Standard B",
            19 => "Standard C",
            20 => "D55",
            21 => "D65",
            22 => "D75",
            23 => "D50",
            _ => return None,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reason `lookup` takes a directory: the same number, two meanings.
    #[test]
    fn tag_one_differs_by_directory() {
        assert_eq!(lookup(IfdKind::Gps, 1).unwrap().name, "GPSLatitudeRef");
        assert_eq!(
            lookup(IfdKind::Interop, 1).unwrap().name,
            "InteroperabilityIndex"
        );
    }

    #[test]
    fn characterization_tags_are_present() {
        for (n, want) in [
            (50721u16, "ColorMatrix1"),
            (50728, "AsShotNeutral"),
            (50714, "BlackLevel"),
            (50719, "DefaultCropOrigin"),
            (50708, "UniqueCameraModel"),
        ] {
            assert_eq!(lookup(IfdKind::Sub(0), n).unwrap().name, want);
        }
    }

    #[test]
    fn unknown_enum_values_return_none_rather_than_guessing() {
        assert!(enum_text("Compression", 60_000).is_none());
        assert_eq!(enum_text("Compression", 1), Some("uncompressed"));
    }

    #[test]
    fn serials_and_locations_are_marked_sensitive() {
        for (ifd, tag) in [
            (IfdKind::Sub(0), 50735u16),
            (IfdKind::Exif, 42033),
            (IfdKind::Exif, 42032),
        ] {
            assert_eq!(lookup(ifd, tag).unwrap().kind, Kind::Sensitive);
        }
        assert_eq!(lookup(IfdKind::Gps, 2).unwrap().kind, Kind::Sensitive);
    }
}
