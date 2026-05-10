pragma ComponentBehavior: Bound
pragma ValueTypeBehavior: Assertable
import QtQuick
import QtQuick.Layouts
import QtQuick.Templates as T
import Qcm.Material as MD
import waywallen.control as WC
import waywallen.ui as W

MD.Page {
    id: root

    W.WallpaperListQuery {
        id: wallpaperQuery
    }

    W.WallpaperScanQuery {
        id: scanQuery
    }

    // Daemon-driven syncs (manual click, LibraryAdd/Remove, startup)
    // all reach the UI through `Notify` (mirrors the daemon's
    // `GlobalEvent` broadcasts). Toast UX is handled here via
    // `Action.toast`; Notify itself is intentionally toast-free.
    Connections {
        target: W.Notify
        function onWallpaperSyncFinished(count, error) {
            if (error && error.length > 0) {
                W.Action.toast("Sync failed: " + error);
            } else {
                W.Action.toast("Scanned " + count + " wallpapers");
            }
            wallpaperQuery.reload();
        }
        function onDaemonReady() {
            root.reloadAll();
        }
    }

    function reloadAll() {
        pluginQuery.reload();
        filterSettingsGet.reload();
    }

    Component.onCompleted: {
        applySort();
        if (W.Notify.daemonPhase === W.Notify.DaemonPhase.Ready)
            reloadAll();
    }

    W.WallpaperApplyQuery {
        id: applyQuery
    }

    W.RendererPluginListQuery {
        id: pluginQuery
    }

    W.LibraryAutoDetectQuery {
        id: autoDetectQuery
    }

    W.SettingsGetQuery {
        id: filterSettingsGet
        onGlobalChanged: {
            wallpaperFilterModel.replaceState(
                        global.wallpaperFilters || [],
                        global.wallpaperFilterLogics || []);
            wallpaperFilterModel.doQuery();
        }
    }

    W.SettingsSetQuery {
        id: filterSettingsSet
    }

    // QAbstractItemModel doesn't auto-expose `count` as a Q_PROPERTY —
    // mirror it here so visibility bindings re-evaluate on row changes.
    property int filterRuleCount: 0
    function _recomputeFilterRuleCount() {
        root.filterRuleCount = wallpaperFilterModel.rowCount();
    }

    Connections {
        target: wallpaperFilterModel
        function onRowsInserted()   { root._recomputeFilterRuleCount(); }
        function onRowsRemoved()    { root._recomputeFilterRuleCount(); }
        function onModelReset()     { root._recomputeFilterRuleCount(); }
        function onLayoutChanged()  { root._recomputeFilterRuleCount(); }
    }

    W.WallpaperFilterRuleModel {
        id: wallpaperFilterModel

        function doQuery() {
            if (!wallpaperQuery.replaceFilterState(items(), filterLogics))
                wallpaperQuery.reload();
        }

        onApply: {
            doQuery();
            const nextGlobal = Object.assign({}, filterSettingsGet.global);
            nextGlobal.wallpaperFilters = items();
            nextGlobal.wallpaperFilterLogics = filterLogics;
            filterSettingsSet.global = nextGlobal;
            filterSettingsSet.plugins = filterSettingsGet.plugins;
            filterSettingsSet.reload();
        }

        onReset: {
            replaceState(
                        filterSettingsGet.global.wallpaperFilters || [],
                        filterSettingsGet.global.wallpaperFilterLogics || []);
            doQuery();
        }
    }

    W.WallpaperFilterDialog {
        id: filterDialog
        parent: T.Overlay.overlay
        model: wallpaperFilterModel
    }

    Connections {
        target: W.Notify
        function onSettingsChanged() {
            filterSettingsGet.reload();
        }
    }

    readonly property var sortOptions: [
        { name: qsTr("Name"),          key: WC.WallpaperSortKey.WALLPAPER_SORT_KEY_NAME },
        { name: qsTr("Size"),          key: WC.WallpaperSortKey.WALLPAPER_SORT_KEY_SIZE },
        { name: qsTr("Last modified"), key: WC.WallpaperSortKey.WALLPAPER_SORT_KEY_LAST_MODIFIED }
    ]
    property int sortIndex: 0
    property bool sortAsc: true
    property WC.wallpaperSortRule emptySortRule

    function applySort() {
        const rule = emptySortRule;
        rule.key = sortOptions[sortIndex].key;
        rule.direction = sortAsc ? WC.SortDirection.SORT_DIRECTION_ASC
                                 : WC.SortDirection.SORT_DIRECTION_DESC;
        wallpaperQuery.sorts = [rule];
    }
    function pickSort(idx) {
        if (idx === sortIndex) {
            sortAsc = !sortAsc;
        } else {
            sortIndex = idx;
            sortAsc = true;
        }
        applySort();
    }

    // Renderers that advertise the selected wallpaper's wp_type, sorted
    // by descending priority. Recomputed on selection or registry change.
    readonly property var rendererCandidates: {
        const wp = root.selectedWallpaper;
        if (!wp) return [];
        const t = wp.wpType || "";
        if (!t) return [];
        const list = (pluginQuery.renderers || []).filter(r => (r.types || []).indexOf(t) >= 0);
        list.sort((a, b) => (b.priority || 0) - (a.priority || 0));
        return list;
    }

    property var selectedWallpaper: null

    // Index into rendererCandidates; reset to 0 whenever the candidate
    // list changes (selection or registry update).
    property int rendererIndex: 0
    onRendererCandidatesChanged: rendererIndex = 0

    // Target display ids for Apply. Empty set = "All displays".
    property var applyTargetIds: []
    function isTargetAll() {
        return applyTargetIds.length === 0;
    }
    function toggleTarget(id) {
        const next = applyTargetIds.slice();
        const i = next.indexOf(id);
        if (i >= 0)
            next.splice(i, 1);
        else
            next.push(id);
        applyTargetIds = next;
    }
    showBackground: false
    padding: MD.MProp.size.isCompact ? 0 : 12

    contentItem: RowLayout {
        spacing: 12

        // --- Left: wallpaper grid ---
        MD.Pane {
            Layout.fillWidth: true
            Layout.fillHeight: true
            radius: root.MD.MProp.page.backgroundRadius
            padding: 0
            showBackground: true

            contentItem: ColumnLayout {
                spacing: 0

                // Toolbar
                RowLayout {
                    Layout.fillWidth: true
                    Layout.leftMargin: 16
                    Layout.rightMargin: 16
                    Layout.topMargin: 12
                    spacing: 8

                    MD.Text {
                        text: "Wallpapers"
                        typescale: MD.Token.typescale.title_large
                        color: MD.Token.color.on_surface
                    }

                    MD.EmbedChip {
                        id: sortChip
                        text: root.sortOptions[root.sortIndex].name
                        trailingIconName: root.sortAsc ? MD.Token.icon.arrow_downward
                                                       : MD.Token.icon.arrow_upward
                        mdState.borderWidth: 1
                        onClicked: sortMenu.open()

                        MD.Menu {
                            id: sortMenu
                            parent: sortChip
                            y: parent.height
                            model: root.sortOptions
                            contentDelegate: MD.MenuItem {
                                required property var modelData
                                required property int index
                                text: modelData.name
                                icon.name: index === root.sortIndex
                                    ? (root.sortAsc ? MD.Token.icon.arrow_downward
                                                    : MD.Token.icon.arrow_upward)
                                    : ' '
                                onClicked: {
                                    root.pickSort(index);
                                    sortMenu.close();
                                }
                            }
                        }
                    }

                    MD.ActionToolBar {
                        Layout.fillWidth: true
                        actions: [
                            MD.Action {
                                icon.name: MD.Token.icon.filter_list
                                text: 'Filters'
                                checked: wallpaperQuery.hasActiveFilters
                                onTriggered: filterDialog.open()
                            },
                            MD.Action {
                                icon.name: MD.Token.icon.hard_drive
                                text: 'Sources'
                                onTriggered: MD.Util.showPopup('waywallen.ui/PagePopup', {
                                    source: 'waywallen.ui/SourceManagePage'
                                }, win)
                            },
                            MD.Action {
                                icon.name: MD.Token.icon.refresh
                                text: 'Refresh'
                                // Disabled while a scan is in flight (the
                                // daemon dedups `scan/refresh` anyway, but
                                // this gives users immediate visual feedback
                                // and avoids stacking ineffective triggers).
                                enabled: !W.Notify.scanInProgress
                                // Daemon answers immediately and pushes
                                // completion via `WallpaperSyncFinished`,
                                // which the `Connections` block on
                                // `Notify` handles (toast + list reload).
                                onTriggered: scanQuery.reload()
                            }
                        ]
                    }
                }

                // Horizontal scan-progress strip below the toolbar.
                // Only shown when the grid has wallpapers to display
                // (the empty-state path uses the centered BusyIndicator).
                MD.LinearIndicator {
                    Layout.fillWidth: true
                    Layout.leftMargin: 16
                    Layout.rightMargin: 16
                    Layout.topMargin: 4
                    visible: m_grid_view.count > 0 && W.Notify.scanInProgress
                    running: visible
                }

                // Grid + centered empty-state overlay
                Item {
                    Layout.fillWidth: true
                    Layout.fillHeight: true

                    MD.VerticalListView {
                        id: m_grid_view
                        anchors.fill: parent
                        clip: true
                        cacheBuffer: 300
                        displayMarginBeginning: 300
                        displayMarginEnd: 300
                        topMargin: 8
                        bottomMargin: 8
                        visible: m_grid_view.count > 0

                        MD.WidthProvider {
                            id: m_wp
                            total: m_grid_view.width
                            minimum: 150
                            spacing: 12
                            leftMargin: 8
                            rightMargin: 8
                        }

                        model: wallpaperQuery.data

                        delegate: WallpaperCard {
                            widthProvider: m_wp
                            onClicked: root.selectedWallpaper = wallpaperQuery.data.item(index)
                        }
                    }

                    ColumnLayout {
                        anchors.centerIn: parent
                        spacing: 16
                        // Wait for the initial list query to settle before
                        // committing to the empty state — otherwise a
                        // brand-new user (empty DB, no libraries) sees a
                        // BusyIndicator flash from the in-flight fetch
                        // even though the daemon isn't scanning anything.
                        visible: m_grid_view.count === 0

                        // Daemon-side scan activity only. The list-fetch
                        // round-trip is a different concern and is gated
                        // by `visible` above.
                        readonly property bool scanning: W.Notify.scanInProgress

                        MD.BusyIndicator {
                            Layout.alignment: Qt.AlignHCenter
                            visible: parent.scanning
                            running: visible
                        }

                        MD.Text {
                            Layout.alignment: Qt.AlignHCenter
                            visible: !parent.scanning
                            text: "No wallpapers found"
                            typescale: MD.Token.typescale.body_large
                            color: MD.Token.color.on_surface_variant
                        }

                        MD.BusyButton {
                            Layout.alignment: Qt.AlignHCenter
                            // Only offer auto-detect when the empty grid is
                            // genuinely "fresh user, nothing configured" —
                            // not when filters are excluding existing rows
                            // and not when libraries are already registered
                            // (in that case the user wants Refresh, not a
                            // second round of auto-detection).
                            visible: !parent.scanning
                                  && root.filterRuleCount === 0
                                  && W.App.libraryManager.count === 0
                            text: "Auto detect libraries"
                            busy: autoDetectQuery.querying
                            mdState.type: MD.Enum.BtFilledTonal
                            onClicked: {
                                if (!busy) autoDetectQuery.reload();
                            }
                        }
                    }
                }
            }
        }

        // --- Right: wallpaper detail panel ---
        MD.Pane {
            Layout.preferredWidth: 280
            Layout.fillHeight: true
            Layout.maximumWidth: 280
            visible: root.selectedWallpaper !== null
            radius: root.MD.MProp.page.backgroundRadius
            padding: 0
            showBackground: true

            contentItem: ColumnLayout {
                spacing: 0

                MD.Flickable {
                    id: m_detail_flick
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    contentHeight: m_detail_col.implicitHeight

                    ColumnLayout {
                        id: m_detail_col
                        width: m_detail_flick.width
                        spacing: 0

                        // Preview
                        W.ThumbnailImage {
                            Layout.fillWidth: true
                            Layout.preferredHeight: visible ? 200 : 0
                            Layout.margins: 12
                            visible: (root.selectedWallpaper?.preview ?? "") !== ""
                                     || (root.selectedWallpaper?.wpType === "video"
                                         && (root.selectedWallpaper?.resource ?? "") !== "")
                            source  : root.selectedWallpaper?.preview ?? ""
                            resource: root.selectedWallpaper?.resource ?? ""
                            wpType  : root.selectedWallpaper?.wpType ?? ""
                            fillMode: Image.PreserveAspectFit
                        }

                        // Info section
                        ColumnLayout {
                            Layout.fillWidth: true
                            Layout.leftMargin: 16
                            Layout.rightMargin: 16
                            Layout.bottomMargin: 16
                            spacing: 12

                        // Close button row
                        RowLayout {
                            Layout.fillWidth: true

                            MD.Text {
                                Layout.fillWidth: true
                                text: root.selectedWallpaper?.name || "Untitled"
                                typescale: MD.Token.typescale.title_large
                                color: MD.Token.color.on_surface
                                wrapMode: Text.Wrap
                                maximumLineCount: 2
                                elide: Text.ElideRight
                            }

                            MD.IconButton {
                                icon.name: MD.Token.icon.close
                                onClicked: root.selectedWallpaper = null
                            }
                        }

                        // Type
                        MD.Text {
                            text: root.selectedWallpaper?.wpType || ""
                            typescale: MD.Token.typescale.label_large
                            color: MD.Token.color.on_surface_variant
                        }

                        // Resource path — show only the last two segments
                        // (parent dir + filename) under a "Path" label.
                        // Full path is exposed via the tooltip / hover.
                        ColumnLayout {
                            Layout.fillWidth: true
                            spacing: 2

                            function shortPath(p) {
                                const parts = (p || "").split("/").filter(s => s.length > 0);
                                return parts.slice(-2).join("/");
                            }

                            MD.Text {
                                text: "Path"
                                typescale: MD.Token.typescale.label_medium
                                color: MD.Token.color.on_surface_variant
                            }
                            MD.Text {
                                Layout.fillWidth: true
                                text: parent.shortPath(root.selectedWallpaper?.resource)
                                typescale: MD.Token.typescale.body_small
                                color: MD.Token.color.on_surface_variant
                                elide: Text.ElideMiddle
                                maximumLineCount: 1
                                wrapMode: Text.NoWrap
                            }
                        }

                        // Media meta block: resolution / size / format.
                        // Hidden entirely when all three values are unknown.
                        ColumnLayout {
                            Layout.fillWidth: true
                            spacing: 4

                            readonly property bool hasResolution: (root.selectedWallpaper?.width ?? 0) !== 0 && (root.selectedWallpaper?.height ?? 0) !== 0
                            readonly property bool hasSize: (root.selectedWallpaper?.size ?? 0) !== 0
                            readonly property bool hasFormat: (root.selectedWallpaper?.format ?? "") !== ""
                            visible: hasResolution || hasSize || hasFormat

                            function formatSize(b) {
                                if (b <= 0) return "";
                                const u = ["B", "KB", "MB", "GB", "TB"];
                                let i = 0;
                                let v = b;
                                while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
                                return v.toFixed(i === 0 ? 0 : 1) + " " + u[i];
                            }

                            MD.Text {
                                text: "Media"
                                typescale: MD.Token.typescale.label_medium
                                color: MD.Token.color.on_surface_variant
                            }

                            GridLayout {
                                Layout.fillWidth: true
                                columns: 2
                                columnSpacing: 12
                                rowSpacing: 2

                                // Resolution row
                                MD.Text {
                                    visible: parent.parent.hasResolution
                                    text: "Resolution"
                                    typescale: MD.Token.typescale.label_medium
                                    color: MD.Token.color.on_surface_variant
                                }
                                MD.Text {
                                    visible: parent.parent.hasResolution
                                    text: (root.selectedWallpaper?.width ?? 0) + "×" + (root.selectedWallpaper?.height ?? 0)
                                    typescale: MD.Token.typescale.body_medium
                                    color: MD.Token.color.on_surface
                                }

                                // Size row
                                MD.Text {
                                    visible: parent.parent.hasSize
                                    text: "Size"
                                    typescale: MD.Token.typescale.label_medium
                                    color: MD.Token.color.on_surface_variant
                                }
                                MD.Text {
                                    visible: parent.parent.hasSize
                                    text: parent.parent.formatSize(root.selectedWallpaper?.size ?? 0)
                                    typescale: MD.Token.typescale.body_medium
                                    color: MD.Token.color.on_surface
                                }

                                // Format row
                                MD.Text {
                                    visible: parent.parent.hasFormat
                                    text: "Format"
                                    typescale: MD.Token.typescale.label_medium
                                    color: MD.Token.color.on_surface_variant
                                }
                                MD.Text {
                                    visible: parent.parent.hasFormat
                                    text: (root.selectedWallpaper?.format ?? "").toLowerCase()
                                    typescale: MD.Token.typescale.body_medium
                                    color: MD.Token.color.on_surface
                                }
                            }
                        }

                        MD.Divider {
                            Layout.fillWidth: true
                        }

                        // Apply target — chip row over DisplayManager.displays
                        // plus a leading "All" chip. Multi-select; empty
                        // selection ⇒ "All" (applied to every display).
                        // Resolution / FPS are resolved daemon-side from
                        // plugin settings, not configured here.
                        ColumnLayout {
                            Layout.fillWidth: true
                            spacing: 4
                            visible: (W.App.displayManager.displays || []).length > 0

                            MD.Text {
                                text: "Apply to"
                                typescale: MD.Token.typescale.label_medium
                                color: MD.Token.color.on_surface_variant
                            }

                            Flow {
                                Layout.fillWidth: true
                                spacing: 6

                                MD.FilterChip {
                                    text: "All"
                                    checked: root.isTargetAll()
                                    onClicked: root.applyTargetIds = []
                                }

                                Repeater {
                                    model: W.App.displayManager.displays

                                    MD.FilterChip {
                                        required property var modelData
                                        text: modelData?.name || ("Display " + modelData?.id)
                                        checked: root.applyTargetIds.indexOf(modelData?.id) >= 0
                                        onClicked: root.toggleTarget(modelData?.id)
                                    }
                                }
                            }
                        }

                        // Renderer pick — only shown when the wallpaper
                        // type has more than one registered renderer.
                        // Single-select chip row; defaults to the highest-
                        // priority candidate (index 0).
                        ColumnLayout {
                            Layout.fillWidth: true
                            spacing: 4
                            visible: root.rendererCandidates.length >= 2

                            MD.Text {
                                text: "Renderer"
                                typescale: MD.Token.typescale.label_medium
                                color: MD.Token.color.on_surface_variant
                            }

                            Flow {
                                Layout.fillWidth: true
                                spacing: 6

                                Repeater {
                                    model: root.rendererCandidates

                                    MD.FilterChip {
                                        required property var modelData
                                        required property int index
                                        text: modelData?.name || ""
                                        checked: root.rendererIndex === index
                                        onClicked: root.rendererIndex = index
                                    }
                                }
                            }
                        }

                        }
                    }
                }

                // Apply controls — sit outside the Flickable so they remain
                // visible regardless of how far the detail content scrolls.
                ColumnLayout {
                    Layout.fillWidth: true
                    Layout.leftMargin: 16
                    Layout.rightMargin: 16
                    Layout.topMargin: 8
                    Layout.bottomMargin: 16
                    spacing: 12

                    // Apply button — disabled when no display is
                    // registered (daemon would reject the call with
                    // FailedPrecondition anyway).
                    MD.BusyButton {
                        id: applyBtn
                        Layout.fillWidth: true
                        text: "Apply"
                        busy: applyQuery.querying
                        mdState.type: MD.Enum.BtFilled
                        enabled: (W.App.displayManager.displays || []).length > 0

                        T.ToolTip.visible: hovered && !enabled
                        T.ToolTip.text: "No display connected"

                        onClicked: {
                            if (busy)
                                return;

                            if (!root.selectedWallpaper)
                                return;
                            applyQuery.wallpaper = root.selectedWallpaper;
                            applyQuery.displayIds = root.applyTargetIds;
                            if (root.rendererCandidates.length >= 2) {
                                const pick = root.rendererCandidates[root.rendererIndex];
                                applyQuery.rendererName = pick ? (pick.name || "") : "";
                            } else {
                                applyQuery.rendererName = "";
                            }
                            applyQuery.reload();
                        }
                    }

                    // Status
                    RowLayout {
                        visible: applyQuery.status === 3
                        spacing: 8

                        MD.Icon {
                            name: MD.Token.icon.check
                            size: 20
                            color: MD.Token.color.primary
                        }
                        MD.Text {
                            text: "Applied"
                            typescale: MD.Token.typescale.label_large
                            color: MD.Token.color.primary
                        }
                    }
                }
            }
        }
    }
}
