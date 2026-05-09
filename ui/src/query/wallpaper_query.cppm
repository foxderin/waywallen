module;
#include "QExtra/macro_qt.hpp"

#ifdef Q_MOC_RUN
#    include "waywallen/query/wallpaper_query.moc"
#endif

export module waywallen:query.wallpaper;
export import :query.query;
export import :model.list_models;

namespace waywallen
{

export class WallpaperListQuery : public QueryList,
                                  public ::QueryExtra<model::WallpaperListModel, WallpaperListQuery> {
    Q_OBJECT
    QML_ELEMENT

    Q_PROPERTY(QString wpType READ wpType WRITE setWpType NOTIFY wpTypeChanged FINAL)
    Q_PROPERTY(QList<waywallen::control::v1::WallpaperFilterRule> filters READ filters WRITE
                   setFilters NOTIFY filtersChanged FINAL)
    Q_PROPERTY(QList<waywallen::control::v1::FilterLogic> filterLogics READ filterLogics WRITE
                   setFilterLogics NOTIFY filterLogicsChanged FINAL)
    Q_PROPERTY(QList<waywallen::control::v1::WallpaperSortRule> sorts READ sorts WRITE
                   setSorts NOTIFY sortsChanged FINAL)
    Q_PROPERTY(bool hasActiveFilters READ hasActiveFilters NOTIFY filtersChanged FINAL)
    Q_PROPERTY(qint32 total READ total NOTIFY totalChanged FINAL)

public:
    WallpaperListQuery(QObject* parent = nullptr);

    auto wpType() const -> const QString&;
    void setWpType(const QString&);

    auto filters() const -> const QList<control::v1::WallpaperFilterRule>&;
    void setFilters(const QList<control::v1::WallpaperFilterRule>&);

    auto filterLogics() const -> const QList<control::v1::FilterLogic>&;
    void setFilterLogics(const QList<control::v1::FilterLogic>&);
    Q_INVOKABLE bool replaceFilterState(const QList<control::v1::WallpaperFilterRule>&,
                                        const QList<control::v1::FilterLogic>&);

    auto sorts() const -> const QList<control::v1::WallpaperSortRule>&;
    void setSorts(const QList<control::v1::WallpaperSortRule>&);

    auto hasActiveFilters() const -> bool;

    auto total() const -> qint32;

    void reload() override;
    void fetchMore(qint32) override;

    Q_SIGNAL void wpTypeChanged();
    Q_SIGNAL void filterStateChanged();
    Q_SIGNAL void filtersChanged();
    Q_SIGNAL void filterLogicsChanged();
    Q_SIGNAL void sortsChanged();
    Q_SIGNAL void totalChanged();

private:
    QString                                   m_wp_type;
    QList<control::v1::WallpaperFilterRule>   m_filters;
    QList<control::v1::FilterLogic>           m_filter_logics;
    QList<control::v1::WallpaperSortRule>     m_sorts;
    qint32                                    m_total { 0 };
};

export class WallpaperScanQuery : public Query, public QueryExtra<control::v1::Response, WallpaperScanQuery> {
    Q_OBJECT
    QML_ELEMENT

    Q_PROPERTY(quint32 count READ count NOTIFY countChanged FINAL)

public:
    WallpaperScanQuery(QObject* parent = nullptr);

    auto count() const -> quint32;

    void reload() override;

    Q_SIGNAL void countChanged();

private:
    quint32 m_count { 0 };
};

export class WallpaperApplyQuery : public Query, public QueryExtra<control::v1::Response, WallpaperApplyQuery> {
    Q_OBJECT
    QML_ELEMENT

    Q_PROPERTY(waywallen::model::Wallpaper wallpaper READ wallpaper WRITE setWallpaper NOTIFY wallpaperChanged FINAL)
    /// Target display ids. Empty list = "apply to all displays" (legacy default).
    Q_PROPERTY(QVariantList displayIds READ displayIds WRITE setDisplayIds NOTIFY displayIdsChanged FINAL)
    /// Optional renderer plugin name. Empty (default) lets the daemon pick
    /// the highest-priority renderer for this wallpaper's type.
    Q_PROPERTY(QString rendererName READ rendererName WRITE setRendererName NOTIFY rendererNameChanged FINAL)
    Q_PROPERTY(QString rendererId READ rendererId NOTIFY rendererIdChanged FINAL)

public:
    WallpaperApplyQuery(QObject* parent = nullptr);

    auto wallpaper() const -> const model::Wallpaper&;
    void setWallpaper(const model::Wallpaper&);

    auto displayIds() const -> const QVariantList&;
    void setDisplayIds(const QVariantList&);

    auto rendererName() const -> const QString&;
    void setRendererName(const QString&);

    auto rendererId() const -> const QString&;

    void reload() override;

    Q_SIGNAL void wallpaperChanged();
    Q_SIGNAL void displayIdsChanged();
    Q_SIGNAL void rendererNameChanged();
    Q_SIGNAL void rendererIdChanged();

private:
    model::Wallpaper m_wallpaper;
    QVariantList     m_display_ids;
    QString          m_renderer_name;
    QString          m_renderer_id;
};

} // namespace waywallen
