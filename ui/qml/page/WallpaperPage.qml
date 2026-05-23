pragma ComponentBehavior: Bound
pragma ValueTypeBehavior: Assertable
import QtQuick
import QtQml as Qml
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

    // After a successful apply the renderer eventually emits
    // `ReportProperties`; re-fetch the detail entry so the
    // UserPropertyPanel picks up the freshly-published schema.
    Connections {
        target: applyQuery
        function onRendererIdChanged() {
            if (applyQuery.rendererId)
                wallpaperGetQuery.reload();
        }
    }

    // Detail panel uses this to fetch the freshest view (tags + media
    // meta) for the currently-selected entry. Reload is auto-triggered
    // when wallpaperId changes.
    W.WallpaperGetQuery {
        id: wallpaperGetQuery
        // `id` is a QML keyword, so qtprotobuf renames `WallpaperEntry.id`
        // to `id_proto`. Using `.id` here would always read undefined.
        wallpaperId: root.selectedWallpaper?.id_proto ?? ""
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
            // Restore sort first so the filter pipeline below doesn't
            // dispatch a list reload with the stale sort: doQuery may
            // route through wallpaperQuery.reload() synchronously when
            // filter state already matches, and m_sorts must already
            // be the persisted value at that point.
            root.restoreSortFromSettings(global.wallpaperSorts || []);
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
            root._persistGlobalChange(g => {
                g.wallpaperFilters = items();
                g.wallpaperFilterLogics = filterLogics;
            });
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
        supportedTypes: pluginQuery.supportedTypes || []
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

    function _buildSortRule() {
        const rule = emptySortRule;
        rule.key = sortOptions[sortIndex].key;
        rule.direction = sortAsc ? WC.SortDirection.SORT_DIRECTION_ASC
                                 : WC.SortDirection.SORT_DIRECTION_DESC;
        return rule;
    }
    function applySort() {
        wallpaperQuery.sorts = [_buildSortRule()];
    }
    // Guard: don't overwrite daemon state with proto defaults when the
    // local mirror of settings hasn't been populated yet. Without this,
    // a click that lands before filterSettingsGet's first response
    // ships a SettingsSet with only the touched field; the daemon then
    // resets target_extent to 0 and clears the filter on commit.
    function _persistGlobalChange(mutator) {
        if (Object.keys(filterSettingsGet.global).length === 0)
            return;
        const nextGlobal = Object.assign({}, filterSettingsGet.global);
        mutator(nextGlobal);
        filterSettingsSet.global = nextGlobal;
        filterSettingsSet.plugins = filterSettingsGet.plugins;
        filterSettingsSet.reload();
    }
    function pickSort(idx) {
        if (idx === sortIndex) {
            sortAsc = !sortAsc;
        } else {
            sortIndex = idx;
            sortAsc = true;
        }
        applySort();
        _persistGlobalChange(g => { g.wallpaperSorts = [_buildSortRule()]; });
    }
    function restoreSortFromSettings(rules) {
        if (!rules || rules.length === 0) {
            // No persisted sort yet — keep whatever defaults are in place
            // and push them down so the list query has at least one rule.
            applySort();
            return;
        }
        const r = rules[0];
        const idx = sortOptions.findIndex(o => o.key === r.key);
        if (idx >= 0) sortIndex = idx;
        sortAsc = r.direction !== WC.SortDirection.SORT_DIRECTION_DESC;
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
                                }, root)
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

                    MD.VerticalGridView {
                        id: m_grid_view
                        anchors.fill: parent
                        clip: true
                        focus: true
                        focusPolicy: Qt.StrongFocus
                        keyNavigationEnabled: true
                        keyNavigationWraps: true
                        currentIndex: -1
                        highlightRangeMode: GridView.NoHighlightRange
                        cacheBuffer: 300
                        displayMarginBeginning: 300
                        displayMarginEnd: 300
                        topMargin: 8
                        bottomMargin: 8
                        leftMargin: 8
                        rightMargin: 8
                        visible: m_grid_view.count > 0

                        readonly property int _cols: Math.max(1, Math.floor(width / 162))
                        cellWidth: (width - leftMargin - rightMargin) / _cols
                        cellHeight: cellWidth

                        model: wallpaperQuery.data

                        delegate: WallpaperCard {
                            onClicked: {
                                m_grid_view.currentIndex = index;
                                root.selectedWallpaper = wallpaperQuery.data.item(index);
                            }
                        }

                        highlightFollowsCurrentItem: true
                        highlight: Component {
                            Item {
                                visible: m_grid_view.currentItem !== null
                                z: 2
                                // Inset 2 = 6 (card margin) − 4 (ring outset),
                                // so the ring sits 4px outside the image
                                // control with the same concentric radius.
                                Rectangle {
                                    anchors.fill: parent
                                    anchors.margins: 2
                                    color: "transparent"
                                    border.color: MD.Token.color.primary
                                    border.width: 3
                                    radius: MD.Token.shape.corner.extra_small + 4
                                }
                            }
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

                // Per-wallpaper user-property edits feed the daemon
                // through a single reused query — propertyKey/value
                // are rewritten on each flush.
                W.WallpaperPropertySetQuery {
                    id: setQuery
                    wallpaperId: root.selectedWallpaper?.id_proto ?? ""
                }

                W.UserPropertyListModel {
                    id: userPropModel
                    schemaJson: wallpaperGetQuery.wallpaper?.userPropertiesSchema ?? ""
                    overridesJson: wallpaperGetQuery.wallpaper?.userPropertyOverrides ?? ""
                }

                // Wire-side write buffer. The model emits one
                // `valueChanged` per user edit; we accumulate the latest
                // per key here and only fire the daemon RPC after the
                // user stops touching things for 200ms.
                QtObject {
                    id: m_pending_writes
                    property var entries: ({})
                }

                Qml.Timer {
                    id: m_flush_timer
                    interval: 200
                    repeat: false
                    onTriggered: {
                        const e = m_pending_writes.entries;
                        for (const k in e) {
                            setQuery.propertyKey = k;
                            setQuery.propertyValue = e[k];
                            setQuery.reload();
                        }
                        m_pending_writes.entries = {};
                    }
                }

                Connections {
                    target: userPropModel
                    function onValueChanged(key, value) {
                        const e = m_pending_writes.entries;
                        e[key] = value;
                        m_pending_writes.entries = e;
                        m_flush_timer.restart();
                    }
                }

                MD.VerticalListView {
                    id: m_detail_view
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    clip: true
                    model: userPropModel
                    spacing: 8
                    leftMargin: 16
                    rightMargin: 16
                    topMargin: 0
                    bottomMargin: 8

                    header: ColumnLayout {
                        width: m_detail_view.contentWidth
                        spacing: 12

                        // Preview
                        W.ThumbnailImage {
                            Layout.fillWidth: true
                            Layout.preferredHeight: visible ? 200 : 0
                            Layout.topMargin: 12
                            visible: (root.selectedWallpaper?.preview ?? "") !== ""
                                     || (["video", "image"].indexOf(root.selectedWallpaper?.wpType ?? "") >= 0
                                         && (root.selectedWallpaper?.resource ?? "") !== "")
                            source  : root.selectedWallpaper?.preview ?? ""
                            resource: root.selectedWallpaper?.resource ?? ""
                            wpType  : root.selectedWallpaper?.wpType ?? ""
                            fillMode: Image.PreserveAspectFit
                        }

                        // Title row
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

                        // Flat key/value grid. Each row hides itself
                        // when the value is unknown so missing fields
                        // collapse out of the layout.
                        GridLayout {
                            id: m_meta
                            Layout.fillWidth: true
                            columns: 2
                            columnSpacing: 12
                            rowSpacing: 4

                            // qtprotobuf marks int64 Q_PROPERTYs as
                            // SCRIPTABLE false, so `wallpaper.size` is
                            // undefined from QML. Read it via the model's
                            // C++ helper instead.
                            readonly property real sizeBytes: wallpaperQuery.data && root.selectedWallpaper
                                                              ? wallpaperQuery.data.sizeOf(root.selectedWallpaper)
                                                              : 0
                            readonly property bool hasPath: (root.selectedWallpaper?.resource ?? "") !== ""
                            readonly property bool hasResolution: Number(root.selectedWallpaper?.width ?? 0) > 0 && Number(root.selectedWallpaper?.height ?? 0) > 0
                            readonly property bool hasSize: sizeBytes > 0
                            readonly property bool hasFormat: (root.selectedWallpaper?.format ?? "") !== ""

                            function shortPath(p) {
                                const parts = (p || "").split("/").filter(s => s.length > 0);
                                return parts.slice(-2).join("/");
                            }
                            function formatSize(b) {
                                let v = Number(b ?? 0);
                                if (!(v > 0)) return "";
                                const u = ["B", "KB", "MB", "GB", "TB"];
                                let i = 0;
                                while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
                                return v.toFixed(i === 0 ? 0 : 1) + " " + u[i];
                            }

                            // Path
                            MD.Text {
                                visible: m_meta.hasPath
                                text: "Path"
                                typescale: MD.Token.typescale.label_medium
                                color: MD.Token.color.on_surface_variant
                            }
                            MD.Text {
                                visible: m_meta.hasPath
                                Layout.fillWidth: true
                                text: m_meta.shortPath(root.selectedWallpaper?.resource)
                                typescale: MD.Token.typescale.body_medium
                                color: MD.Token.color.on_surface
                                elide: Text.ElideMiddle
                                maximumLineCount: 1
                                wrapMode: Text.NoWrap
                            }

                            // Resolution
                            MD.Text {
                                visible: m_meta.hasResolution
                                text: "Resolution"
                                typescale: MD.Token.typescale.label_medium
                                color: MD.Token.color.on_surface_variant
                            }
                            MD.Text {
                                visible: m_meta.hasResolution
                                text: (root.selectedWallpaper?.width ?? 0) + "×" + (root.selectedWallpaper?.height ?? 0)
                                typescale: MD.Token.typescale.body_medium
                                color: MD.Token.color.on_surface
                            }

                            // Size
                            MD.Text {
                                visible: m_meta.hasSize
                                text: "Size"
                                typescale: MD.Token.typescale.label_medium
                                color: MD.Token.color.on_surface_variant
                            }
                            MD.Text {
                                visible: m_meta.hasSize
                                text: m_meta.formatSize(m_meta.sizeBytes)
                                typescale: MD.Token.typescale.body_medium
                                color: MD.Token.color.on_surface
                            }

                            // Format
                            MD.Text {
                                visible: m_meta.hasFormat
                                text: "Format"
                                typescale: MD.Token.typescale.label_medium
                                color: MD.Token.color.on_surface_variant
                            }
                            MD.Text {
                                visible: m_meta.hasFormat
                                text: (root.selectedWallpaper?.format ?? "").toLowerCase()
                                typescale: MD.Token.typescale.body_medium
                                color: MD.Token.color.on_surface
                            }
                        }

                        // Tags. Sourced from the per-item query
                        // (wallpaperGetQuery) so the panel reflects DB
                        // edits even if the list page is stale.
                        Flow {
                            Layout.fillWidth: true
                            spacing: 6
                            visible: (wallpaperGetQuery.wallpaper?.tags?.length ?? 0) > 0
                            Repeater {
                                model: wallpaperGetQuery.wallpaper?.tags ?? []
                                delegate: MD.AssistChip {
                                    required property string modelData
                                    text: modelData
                                }
                            }
                        }

                        // Description (project.json `description`) — collapsed
                        // to a fixed line count by default; user clicks the
                        // chevron to expand. Source string is Steam Workshop
                        // BBCode + bare URLs + `\n` line breaks; the C++
                        // `W.Util.bbcodeToHtml` helper converts it to the
                        // Qt.StyledText HTML subset before display.
                        ColumnLayout {
                            id: m_description
                            Layout.fillWidth: true
                            spacing: 4
                            visible: (wallpaperGetQuery.wallpaper?.description ?? "") !== ""

                            property bool expanded: false
                            // Collapsed view shows 3 lines; expanded shows all.
                            readonly property int collapsedLines: 3

                            MD.Divider {
                                Layout.fillWidth: true
                            }

                            RowLayout {
                                Layout.fillWidth: true
                                spacing: 4

                                MD.Text {
                                    Layout.fillWidth: true
                                    text: "Description"
                                    typescale: MD.Token.typescale.label_large
                                    color: MD.Token.color.on_surface_variant
                                }

                                MD.IconButton {
                                    icon.name: m_description.expanded ? MD.Token.icon.expand_less
                                                                       : MD.Token.icon.expand_more
                                    visible: m_descText.lineCount > m_description.collapsedLines
                                          || m_description.expanded
                                    onClicked: m_description.expanded = !m_description.expanded
                                }
                            }

                            MD.Text {
                                id: m_descText
                                Layout.fillWidth: true
                                text: W.Util.bbcodeToHtml(
                                    wallpaperGetQuery.wallpaper?.description ?? "")
                                textFormat: Text.StyledText
                                typescale: MD.Token.typescale.body_medium
                                color: MD.Token.color.on_surface
                                wrapMode: Text.WordWrap
                                maximumLineCount: m_description.expanded
                                                  ? Number.MAX_SAFE_INTEGER
                                                  : m_description.collapsedLines
                                elide: m_description.expanded ? Text.ElideNone
                                                              : Text.ElideRight
                                onLinkActivated: link => Qt.openUrlExternally(link)
                            }
                        }

                        // "Properties" section header — sits inside the
                        // ListView's `header` so the title hides cleanly
                        // when the model is empty.
                        ColumnLayout {
                            Layout.fillWidth: true
                            spacing: 4
                            visible: userPropModel.count > 0

                            MD.Divider { Layout.fillWidth: true }

                            RowLayout {
                                Layout.fillWidth: true
                                spacing: 4

                                MD.Text {
                                    Layout.fillWidth: true
                                    text: "Properties"
                                    typescale: MD.Token.typescale.label_large
                                    color: MD.Token.color.on_surface_variant
                                }

                                MD.IconButton {
                                    icon.name: MD.Token.icon.restart_alt
                                    mdState.size: MD.Enum.XS
                                    onClicked: userPropModel.resetAll()

                                    MD.ToolTip {
                                        visible: parent.hovered
                                        text: "Reset to defaults"
                                    }
                                }
                            }
                        }
                    }

                    // Per-property delegate. owe-supported types
                    // (color / slider / bool) draw their native editor;
                    // anything else is a disabled label so the user
                    // knows the property exists.
                    delegate: ColumnLayout {
                        id: m_prop_delegate
                        required property string key
                        required property string label
                        required property string type
                        required property bool   supported
                        required property real   minVal
                        required property real   maxVal
                        required property string currentValue
                        required property bool   hasAlpha

                        width: ListView.view ? (ListView.view.width - ListView.view.leftMargin - ListView.view.rightMargin) : 0
                        spacing: 2

                        MD.Text {
                            text: m_prop_delegate.label
                            textFormat: Text.StyledText
                            typescale: MD.Token.typescale.label_medium
                            color: MD.Token.color.on_surface
                            Layout.fillWidth: true
                            wrapMode: Text.WordWrap
                            onLinkActivated: link => Qt.openUrlExternally(link)
                        }

                        // Bool → switch.
                        // Plain `checked: …` bindings get severed the first
                        // time the control writes its own state (Switch
                        // toggle on click, Slider drag, ColorPicker accept).
                        // Use Binding so model-driven changes (esp. Reset)
                        // still flow back into the control afterwards.
                        MD.Switch {
                            id: m_switch
                            visible: m_prop_delegate.type === "bool"
                            onToggled: userPropModel.setValue(m_prop_delegate.key,
                                                              checked ? "true" : "false")
                        }
                        Binding {
                            target: m_switch
                            property: "checked"
                            value: m_prop_delegate.currentValue === "true"
                        }

                        // Slider → MD.Slider with right-aligned readout
                        RowLayout {
                            visible: m_prop_delegate.type === "slider"
                            Layout.fillWidth: true
                            spacing: 8
                            MD.Slider {
                                id: m_slider
                                Layout.fillWidth: true
                                from: m_prop_delegate.minVal
                                to:   m_prop_delegate.maxVal
                                onMoved: userPropModel.setValue(m_prop_delegate.key, String(value))
                            }
                            MD.Text {
                                text: Number(m_prop_delegate.currentValue).toFixed(3)
                                typescale: MD.Token.typescale.body_small
                                color: MD.Token.color.on_surface_variant
                                Layout.preferredWidth: 56
                                horizontalAlignment: Text.AlignRight
                            }
                        }
                        Binding {
                            target: m_slider
                            property: "value"
                            value: Number(m_prop_delegate.currentValue)
                        }

                        // Color → MD.ColorPickerButton; alpha surfaces
                        // only when the wire value already had 4 floats
                        // (WE almost always emits RGB).
                        MD.ColorPickerButton {
                            id: m_color
                            visible: m_prop_delegate.type === "color"
                            Layout.preferredWidth: 80
                            Layout.preferredHeight: 32
                            showAlpha: m_prop_delegate.hasAlpha
                            onAccepted: c => userPropModel.setValue(
                                m_prop_delegate.key,
                                W.Util.colorToWire(c, showAlpha))
                        }
                        Binding {
                            target: m_color
                            property: "color"
                            value: W.Util.colorFromWire(m_prop_delegate.currentValue)
                        }

                        // Unsupported owe types: disabled row so users see
                        // the property exists, but editing is a no-op.
                        MD.Text {
                            visible: !m_prop_delegate.supported
                            text: "(" + m_prop_delegate.type + " — not yet supported)"
                            typescale: MD.Token.typescale.body_small
                            color: MD.Token.color.on_surface_variant
                        }
                    }
                }

                // Footer: pinned outside the Flickable so the Apply controls
                // — including target / renderer selectors — stay visible
                // regardless of how far the detail content scrolls.
                ColumnLayout {
                    Layout.fillWidth: true
                    Layout.leftMargin: 16
                    Layout.rightMargin: 16
                    Layout.topMargin: 8
                    Layout.bottomMargin: 8
                    spacing: 8

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

                        MD.ToolTip {
                            visible: applyBtn.hovered && !applyBtn.enabled
                            text: "No display connected"
                        }

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
