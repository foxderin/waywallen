module;
#include "control.qpb.h"

export module waywallen:proto;

namespace proto = waywallen::control::v1;

export namespace waywallen::control::v1
{
using proto::StatusGadget::Status;

using proto::Request;
using proto::Response;
using proto::ServerFrame;
using proto::Event;
using proto::DisplaySnapshot;
using proto::DisplayChanged;
using proto::DisplayRemoved;
using proto::Empty;

using proto::HealthRequest;
using proto::HealthResponse;

using proto::RendererSpawnRequest;
using proto::RendererSpawnResponse;
using proto::RendererListRequest;
using proto::RendererListResponse;
using proto::RendererInstance;
using proto::RendererPlayRequest;
using proto::RendererPauseRequest;
using proto::RendererMouseRequest;
using proto::RendererFpsRequest;
using proto::RendererKillRequest;

using proto::RendererPluginListRequest;
using proto::RendererPluginListResponse;
using proto::RendererPluginInfo;
using proto::SettingSchema;

using proto::WallpaperEntry;
using proto::WallpaperListRequest;
using proto::WallpaperListResponse;
using proto::WallpaperScanRequest;
using proto::WallpaperScanResponse;
using proto::WallpaperSyncFinished;
using proto::WallpaperApplyRequest;
using proto::WallpaperApplyResponse;
using proto::WallpaperGetRequest;
using proto::WallpaperGetResponse;
using proto::WallpaperPropertySetRequest;
using proto::WallpaperPropertySetResponse;

using proto::StatusSync;
using proto::DaemonPhaseGadget::DaemonPhase;

using proto::SourceListRequest;
using proto::SourceListResponse;
using proto::SourcePluginInfo;

using proto::DisplayInfo;
using proto::DisplayLinkInfo;
using proto::DisplayListRequest;
using proto::DisplayListResponse;
using proto::LayoutOverride;
using proto::DisplayLayoutSetRequest;
using proto::DisplayLayoutSetResponse;

using proto::GpuInfo;
using proto::GpuListRequest;
using proto::GpuListResponse;

using proto::LibraryInstance;
using proto::LibraryListRequest;
using proto::LibraryListResponse;
using proto::LibraryAddRequest;
using proto::LibraryRemoveRequest;
using proto::LibraryAutoDetectRequest;
using proto::LibraryAutoDetectResponse;
using proto::LibrarySnapshot;
using proto::LibraryChanged;
using proto::LibraryRemoved;

using proto::GlobalSettings;
using proto::PluginSettings;
using proto::SettingsGetRequest;
using proto::SettingsGetResponse;
using proto::SettingsSetRequest;
using proto::SettingsChanged;
using proto::LayoutPrefs;
using proto::FillModeGadget::FillMode;
using proto::AlignGadget::Align;
using proto::RotationGadget::Rotation;
using proto::RenderSizePolicyGadget::RenderSizePolicy;

using proto::WallpaperFilterRule;
using proto::WallpaperFilterTypeGadget::WallpaperFilterType;
using proto::WallpaperStringFilter;
using proto::WallpaperIntFilter;
using proto::StringConditionGadget::StringCondition;
using proto::IntConditionGadget::IntCondition;
using proto::LogicOpGadget::LogicOp;
using proto::FilterLogic;
using proto::WallpaperSortRule;
using proto::WallpaperSortKeyGadget::WallpaperSortKey;
using proto::SortDirectionGadget::SortDirection;
} // namespace waywallen::control::v1
