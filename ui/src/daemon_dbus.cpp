module;
#include "waywallen/daemon_dbus.moc.h"

#include <csignal>
#include <sys/types.h>

#include <QtCore/QCoreApplication>
#include <QtCore/QDebug>
#include <QtCore/QDir>
#include <QtCore/QFile>
#include <QtCore/QProcess>
#include <QtCore/QVariant>
#include <QtDBus/QDBusConnection>
#include <QtDBus/QDBusConnectionInterface>
#include <QtDBus/QDBusInterface>
#include <QtDBus/QDBusMessage>
#include <QtDBus/QDBusReply>
#include <QtDBus/QDBusServiceWatcher>

module waywallen;
import :daemon_dbus;

namespace waywallen
{

namespace {

constexpr const char* kBusName     = "org.waywallen.waywallen.Daemon";
constexpr const char* kObjectPath  = "/org/waywallen/waywallen/Daemon";
constexpr const char* kInterface   = "org.waywallen.waywallen.Daemon1";
constexpr const char* kPropsIface  = "org.freedesktop.DBus.Properties";


DaemonDBusClient* g_instance { nullptr };

bool is_unknown_property_error(const QString& name) {
    // Different DBus stacks report the missing-property condition with
    // slightly different error names; accept both.
    return name == QLatin1String("org.freedesktop.DBus.Error.UnknownProperty") ||
           name == QLatin1String("org.freedesktop.DBus.Error.InvalidArgs");
}

QVariant unwrap_variant(QVariant v) {
    if (v.canConvert<QDBusVariant>()) {
        v = v.value<QDBusVariant>().variant();
    }
    return v;
}

} // namespace

DaemonDBusClient* DaemonDBusClient::create(QQmlEngine*, QJSEngine*) {
    auto* inst = instance();
    QJSEngine::setObjectOwnership(inst, QJSEngine::CppOwnership);
    return inst;
}

DaemonDBusClient* DaemonDBusClient::instance() {
    if (! g_instance) {
        g_instance = new DaemonDBusClient();
    }
    return g_instance;
}

DaemonDBusClient::DaemonDBusClient(QObject* parent)
    : QObject(parent), m_bus(QDBusConnection::sessionBus()) {
    if (! g_instance) {
        g_instance = this;
    }

    if (! m_bus.isConnected()) {
        qWarning("DaemonDBusClient: session bus not connected: %s",
                 qPrintable(m_bus.lastError().message()));
        return;
    }

    setup_subscriptions();

    // Initial probe — refreshWsPort drives the full state-machine update.
    auto iface = m_bus.interface();
    bool registered =
        iface && iface->isServiceRegistered(QString::fromLatin1(kBusName)).value();
    if (registered) {
        refreshWsPort();
    } else {
        set_status(Disconnected);
    }
}

DaemonDBusClient::~DaemonDBusClient() {
    if (g_instance == this) {
        g_instance = nullptr;
    }
}

void DaemonDBusClient::setup_subscriptions() {
    m_watcher = new QDBusServiceWatcher(QString::fromLatin1(kBusName),
                                        m_bus,
                                        QDBusServiceWatcher::WatchForRegistration
                                            | QDBusServiceWatcher::WatchForUnregistration,
                                        this);
    connect(m_watcher, &QDBusServiceWatcher::serviceRegistered, this,
            &DaemonDBusClient::on_service_registered);
    connect(m_watcher, &QDBusServiceWatcher::serviceUnregistered, this,
            &DaemonDBusClient::on_service_unregistered);

    bool ok = m_bus.connect(QString::fromLatin1(kBusName),
                            QString::fromLatin1(kObjectPath),
                            QString::fromLatin1(kInterface),
                            QStringLiteral("Ready"),
                            this,
                            SLOT(on_ready()));
    if (! ok) {
        qWarning("DaemonDBusClient: failed to subscribe to Ready signal");
    }

    ok = m_bus.connect(QString::fromLatin1(kBusName),
                       QString::fromLatin1(kObjectPath),
                       QString::fromLatin1(kInterface),
                       QStringLiteral("ShuttingDown"),
                       this,
                       SLOT(on_shutting_down()));
    if (! ok) {
        qWarning("DaemonDBusClient: failed to subscribe to ShuttingDown signal");
    }

    ok = m_bus.connect(QString::fromLatin1(kBusName),
                       QString::fromLatin1(kObjectPath),
                       QString::fromLatin1(kPropsIface),
                       QStringLiteral("PropertiesChanged"),
                       this,
                       SLOT(on_properties_changed(QString, QVariantMap, QStringList)));
    if (! ok) {
        qWarning("DaemonDBusClient: failed to subscribe to PropertiesChanged");
    }
}

QDBusMessage DaemonDBusClient::call_get(const QString& prop) {
    QDBusMessage msg = QDBusMessage::createMethodCall(QString::fromLatin1(kBusName),
                                                     QString::fromLatin1(kObjectPath),
                                                     QString::fromLatin1(kPropsIface),
                                                     QStringLiteral("Get"));
    msg << QString::fromLatin1(kInterface) << prop;
    return m_bus.call(msg, QDBus::Block, 2000);
}

quint16 DaemonDBusClient::refreshWsPort() {
    if (! m_bus.isConnected()) {
        set_ws_port(0);
        set_status(Disconnected);
        return 0;
    }

    // Step 1: WsPort. Failure here means daemon is gone / unreachable.
    QDBusMessage port_reply = call_get(QStringLiteral("WsPort"));
    if (port_reply.type() != QDBusMessage::ReplyMessage) {
        qDebug("DaemonDBusClient: WsPort read failed: %s",
               qPrintable(port_reply.errorMessage()));
        set_ws_port(0);
        set_status(Disconnected);
        return 0;
    }
    {
        const auto args = port_reply.arguments();
        if (! args.isEmpty()) {
            bool ok = false;
            quint16 port = static_cast<quint16>(unwrap_variant(args.front()).toUInt(&ok));
            if (ok) set_ws_port(port);
        }
    }

    // Step 2: Version. Best-effort — UnknownProperty/InvalidArgs maps to
    // VersionMissing (old daemon predating the version handshake).
    QDBusMessage ver_reply = call_get(QStringLiteral("Version"));
    if (ver_reply.type() != QDBusMessage::ReplyMessage) {
        if (is_unknown_property_error(ver_reply.errorName())) {
            m_daemon_version.clear();
            set_status(VersionMissing);
        } else {
            qDebug("DaemonDBusClient: Version read failed: %s (%s)",
                   qPrintable(ver_reply.errorName()),
                   qPrintable(ver_reply.errorMessage()));
            set_status(Disconnected);
        }
        return m_ws_port;
    }
    {
        const auto args = ver_reply.arguments();
        if (args.isEmpty()) {
            m_daemon_version.clear();
            set_status(VersionMissing);
            return m_ws_port;
        }
        const QString version = unwrap_variant(args.front()).toString();
        m_daemon_version = version;
        set_status(version == QCoreApplication::applicationVersion()
                       ? Connected : VersionMismatch);
    }
    return m_ws_port;
}

bool DaemonDBusClient::launchDaemon() {
    qDebug("DaemonDBusClient: launching daemon (QProcess::startDetached)");
    bool ok = QProcess::startDetached(QStringLiteral("waywallen"), {});
    if (! ok) {
        qWarning("DaemonDBusClient: failed to start waywallen");
    }
    return ok;
}

QVariantList DaemonDBusClient::listWaywallenProcesses() {
    QVariantList out;
    QDir proc(QStringLiteral("/proc"));
    const QStringList entries = proc.entryList(QDir::Dirs | QDir::NoDotAndDotDot);
    for (const QString& entry : entries) {
        bool is_pid = false;
        const quint32 pid = entry.toUInt(&is_pid);
        if (! is_pid) continue;

        QFile comm_file(QStringLiteral("/proc/%1/comm").arg(entry));
        if (! comm_file.open(QIODevice::ReadOnly)) continue;
        QByteArray comm = comm_file.readAll().trimmed();
        if (comm != QByteArrayLiteral("waywallen")) continue;

        QFile cmdline_file(QStringLiteral("/proc/%1/cmdline").arg(entry));
        QString cmdline;
        if (cmdline_file.open(QIODevice::ReadOnly)) {
            QByteArray raw = cmdline_file.readAll();
            // /proc cmdline is NUL-separated argv with a trailing NUL.
            for (char& c : raw) {
                if (c == '\0') c = ' ';
            }
            cmdline = QString::fromLocal8Bit(raw).trimmed();
        }
        if (cmdline.isEmpty()) cmdline = QString::fromLatin1(comm);

        QVariantMap row;
        row.insert(QStringLiteral("pid"), pid);
        row.insert(QStringLiteral("cmdline"), cmdline);
        out.append(row);
    }
    return out;
}

bool DaemonDBusClient::killProcess(quint32 pid) {
    if (pid == 0) return false;
    if (::kill(static_cast<pid_t>(pid), SIGTERM) != 0) {
        qWarning("DaemonDBusClient: kill(%u, SIGTERM) failed: %s",
                 pid, qPrintable(QString::fromLocal8Bit(strerror(errno))));
        return false;
    }
    return true;
}

void DaemonDBusClient::on_service_registered(const QString& service) {
    if (service != QString::fromLatin1(kBusName)) return;
    qDebug("DaemonDBusClient: daemon registered on bus");
    refreshWsPort();
}

void DaemonDBusClient::on_service_unregistered(const QString& service) {
    if (service != QString::fromLatin1(kBusName)) return;
    qDebug("DaemonDBusClient: daemon unregistered from bus");
    set_ws_port(0);
    m_daemon_version.clear();
    set_status(Disconnected);
}

void DaemonDBusClient::on_ready() {
    qDebug("DaemonDBusClient: Ready signal received");
    refreshWsPort();
}

void DaemonDBusClient::on_shutting_down() {
    qDebug("DaemonDBusClient: ShuttingDown signal received");
    set_status(Disconnected);
    // Keep m_ws_port until NameOwnerChanged confirms the unregister.
}

void DaemonDBusClient::on_properties_changed(const QString&     iface,
                                             const QVariantMap& changed,
                                             const QStringList& /*invalidated*/) {
    if (iface != QString::fromLatin1(kInterface)) return;
    auto it = changed.find(QStringLiteral("WsPort"));
    if (it == changed.end()) return;
    bool ok = false;
    quint16 port = static_cast<quint16>(unwrap_variant(it.value()).toUInt(&ok));
    if (ok) {
        set_ws_port(port);
    }
}

void DaemonDBusClient::set_status(Status s) {
    if (m_status == s) return;
    m_status = s;
    Q_EMIT statusChanged();
}

void DaemonDBusClient::set_ws_port(quint16 port) {
    if (m_ws_port == port) return;
    m_ws_port = port;
    Q_EMIT wsPortChanged(m_ws_port);
}

} // namespace waywallen

#include "waywallen/daemon_dbus.moc"
