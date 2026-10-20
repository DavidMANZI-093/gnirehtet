package android.util;

/**
 * Minimal no-op stub of android.util.Log for JVM unit tests.
 *
 * Main sources are compiled against the real android.jar, but at test runtime
 * this class shadows android.util.Log (it is first on the classpath), so
 * logging calls in the classes under test do not hit the android.jar
 * "Stub!" implementations.
 */
public final class Log {
    public static final int VERBOSE = 2;
    public static final int DEBUG = 3;
    public static final int INFO = 4;
    public static final int WARN = 5;
    public static final int ERROR = 6;

    public static int v(String tag, String msg) {
        return 0;
    }

    public static int v(String tag, String msg, Throwable tr) {
        return 0;
    }

    public static int d(String tag, String msg) {
        return 0;
    }

    public static int d(String tag, String msg, Throwable tr) {
        return 0;
    }

    public static int i(String tag, String msg) {
        return 0;
    }

    public static int i(String tag, String msg, Throwable tr) {
        return 0;
    }

    public static int w(String tag, String msg) {
        return 0;
    }

    public static int w(String tag, String msg, Throwable tr) {
        return 0;
    }

    public static int w(String tag, Throwable tr) {
        return 0;
    }

    public static int e(String tag, String msg) {
        return 0;
    }

    public static int e(String tag, String msg, Throwable tr) {
        return 0;
    }

    public static int println(int priority, String tag, String msg) {
        return 0;
    }

    public static boolean isLoggable(String tag, int level) {
        return false;
    }

    private Log() {
    }
}
