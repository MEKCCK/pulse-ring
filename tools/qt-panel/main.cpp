// pulse-ring 控制面板 —— Qt Quick（C++ 宿主 + QML 界面）
// 提供文件读写 / 目录列表 / 服务重启，供 QML 调用。
#include <QGuiApplication>
#include <QQmlApplicationEngine>
#include <QQmlContext>
#include <QObject>
#include <QFile>
#include <QDir>
#include <QProcess>
#include <QDebug>
#include <QStandardPaths>

class FileIO : public QObject {
    Q_OBJECT
public:
    Q_INVOKABLE QString readFile(const QString &path) const {
        QFile f(path);
        if (!f.open(QIODevice::ReadOnly)) return QString();
        return QString::fromUtf8(f.readAll());
    }
    Q_INVOKABLE bool writeFile(const QString &path, const QString &content) const {
        QFile f(path);
        if (!f.open(QIODevice::WriteOnly | QIODevice::Truncate)) return false;
        f.write(content.toUtf8());
        f.close();
        return true;
    }
    Q_INVOKABLE QStringList listDir(const QString &path) const {
        QDir d(path);
        if (!d.exists()) return QStringList();
        return d.entryList(QDir::Dirs | QDir::NoDotAndDotDot);
    }
    Q_INVOKABLE bool exists(const QString &path) const { return QFile::exists(path); }
    Q_INVOKABLE void restartService() const {
        // 杀掉并重启 pulse-ring（配置热重载解决不了时用）。
        QProcess::startDetached("pkill", QStringList() << "-9" << "-x" << "pulse-ring");
        QProcess::startDetached("bash", QStringList()
            << "-c" << "sleep 1; RUST_LOG=info nohup ~/.local/bin/pulse-ring > /tmp/pulse-ring.log 2>&1 < /dev/null &");
    }
    Q_INVOKABLE bool serviceRunning() const {
        QProcess p;
        p.start("pgrep", QStringList() << "-x" << "pulse-ring");
        p.waitForFinished(2000);
        return p.exitCode() == 0;
    }
    Q_INVOKABLE QString homeDir() const { return QDir::homePath(); }
};

int main(int argc, char *argv[]) {
    QGuiApplication app(argc, argv);
    app.setApplicationName("pulse-ring-panel");
    QQmlApplicationEngine engine;
    FileIO io;
    engine.rootContext()->setContextProperty("FileIO", &io);
    const QUrl url(QStringLiteral("qrc:/Main.qml"));
    QObject::connect(&engine, &QQmlApplicationEngine::objectCreated,
                     &app, [url](QObject *obj, const QUrl &objUrl) {
        if (!obj && url == objUrl) QCoreApplication::exit(-1);
    }, Qt::QueuedConnection);
    engine.load(url);
    return app.exec();
}

#include "main.moc"
