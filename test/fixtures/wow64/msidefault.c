/* test/wow64/msidefault: Enter in an installer's dialog takes the button a
 * ControlCondition names the default (its Default action), not the Dialog
 * table's Control_Default. A package of its own in %TEMP%, with one modal
 * dialog of two push buttons: Control_Default "exit", and a Default
 * condition, true, on "next". Each button sets CHOSEN to its name and ends
 * the dialog; a thread presses Enter in it once it shows. Prints
 * "msidefault ok" when Enter took "next". */
#include <windows.h>
#include <msi.h>
#include <msiquery.h>
#include <stdio.h>

static UINT run( MSIHANDLE db, const char *sql )
{
    MSIHANDLE view;
    UINT r = MsiDatabaseOpenViewA( db, sql, &view );
    if (r) { printf( "msidefault: %u for %s\n", r, sql ); return r; }
    r = MsiViewExecute( view, 0 );
    MsiViewClose( view );
    MsiCloseHandle( view );
    if (r) printf( "msidefault: %u running %s\n", r, sql );
    return r;
}

static DWORD WINAPI press_enter( void *arg )
{
    HWND hwnd = NULL;
    int i;

    for (i = 0; i < 1500 && !hwnd; i++)
        if (!(hwnd = FindWindowA( "MsiDialogCloseClass", NULL ))) Sleep( 20 );
    if (!hwnd) return 1;
    Sleep( 500 );
    PostMessageA( hwnd, WM_KEYDOWN, VK_RETURN, 0x1c0001 );
    PostMessageA( hwnd, WM_KEYUP, VK_RETURN, 0xc01c0001 );
    return 0;
}

int main( void )
{
    static const char *sql[] = {
        "CREATE TABLE `Property` (`Property` CHAR(72) NOT NULL, `Value` CHAR(0) NOT NULL PRIMARY KEY `Property`)",
        "INSERT INTO `Property` (`Property`, `Value`) VALUES ('ProductCode', '{2D1A9C47-5E0B-4F6A-9C3E-7B8D1E2F3A40}')",
        "INSERT INTO `Property` (`Property`, `Value`) VALUES ('ProductLanguage', '1033')",
        "CREATE TABLE `Dialog` (`Dialog` CHAR(72) NOT NULL, `HCentering` SHORT NOT NULL, `VCentering` SHORT NOT NULL, "
            "`Width` SHORT NOT NULL, `Height` SHORT NOT NULL, `Attributes` LONG, `Title` CHAR(128) LOCALIZABLE, "
            "`Control_First` CHAR(50) NOT NULL, `Control_Default` CHAR(50), `Control_Cancel` CHAR(50) PRIMARY KEY `Dialog`)",
        "INSERT INTO `Dialog` (`Dialog`, `HCentering`, `VCentering`, `Width`, `Height`, `Attributes`, `Title`, `Control_First`, `Control_Default`) "
            "VALUES ('pick', 50, 50, 100, 60, 3, 'msidefault', 'exit', 'exit')",
        "CREATE TABLE `Control` (`Dialog_` CHAR(72) NOT NULL, `Control` CHAR(50) NOT NULL, `Type` CHAR(20) NOT NULL, "
            "`X` SHORT NOT NULL, `Y` SHORT NOT NULL, `Width` SHORT NOT NULL, `Height` SHORT NOT NULL, `Attributes` LONG, "
            "`Property` CHAR(50), `Text` CHAR(0) LOCALIZABLE, `Control_Next` CHAR(50), `Help` CHAR(255) LOCALIZABLE "
            "PRIMARY KEY `Dialog_`, `Control`)",
        "INSERT INTO `Control` (`Dialog_`, `Control`, `Type`, `X`, `Y`, `Width`, `Height`, `Attributes`, `Text`) "
            "VALUES ('pick', 'exit', 'PushButton', 10, 10, 66, 17, 3, 'Exit')",
        "INSERT INTO `Control` (`Dialog_`, `Control`, `Type`, `X`, `Y`, `Width`, `Height`, `Attributes`, `Text`) "
            "VALUES ('pick', 'next', 'PushButton', 10, 30, 66, 17, 3, 'Next')",
        "CREATE TABLE `ControlEvent` (`Dialog_` CHAR(72) NOT NULL, `Control_` CHAR(50) NOT NULL, `Event` CHAR(50) NOT NULL, "
            "`Argument` CHAR(255) NOT NULL, `Condition` CHAR(255), `Ordering` SHORT "
            "PRIMARY KEY `Dialog_`, `Control_`, `Event`, `Argument`, `Condition`)",
        "INSERT INTO `ControlEvent` (`Dialog_`, `Control_`, `Event`, `Argument`, `Condition`, `Ordering`) VALUES ('pick', 'exit', '[CHOSEN]', 'exit', '1', 1)",
        "INSERT INTO `ControlEvent` (`Dialog_`, `Control_`, `Event`, `Argument`, `Condition`, `Ordering`) VALUES ('pick', 'exit', 'EndDialog', 'Exit', '1', 2)",
        "INSERT INTO `ControlEvent` (`Dialog_`, `Control_`, `Event`, `Argument`, `Condition`, `Ordering`) VALUES ('pick', 'next', '[CHOSEN]', 'next', '1', 1)",
        "INSERT INTO `ControlEvent` (`Dialog_`, `Control_`, `Event`, `Argument`, `Condition`, `Ordering`) VALUES ('pick', 'next', 'EndDialog', 'Exit', '1', 2)",
        "CREATE TABLE `ControlCondition` (`Dialog_` CHAR(72) NOT NULL, `Control_` CHAR(50) NOT NULL, `Action` CHAR(50) NOT NULL, "
            "`Condition` CHAR(255) NOT NULL PRIMARY KEY `Dialog_`, `Control_`, `Action`, `Condition`)",
        "INSERT INTO `ControlCondition` (`Dialog_`, `Control_`, `Action`, `Condition`) VALUES ('pick', 'next', 'Default', '1')",
    };
    char path[MAX_PATH], pkgname[16], chosen[16] = "";
    DWORD size = sizeof(chosen), code = 1;
    MSIHANDLE db, sum, pkg;
    HANDLE thread;
    UINT r, i;

    setvbuf( stdout, NULL, _IONBF, 0 );
    GetTempPathA( sizeof(path), path );
    lstrcatA( path, "msidefault.msi" );
    DeleteFileA( path );
    if ((r = MsiOpenDatabaseA( path, MSIDBOPEN_CREATE, &db ))) { printf( "msidefault: creating %s: %u\n", path, r ); return 1; }
    if ((r = MsiGetSummaryInformationA( db, NULL, 7, &sum ))) { printf( "msidefault: summary %u\n", r ); return 1; }
    MsiSummaryInfoSetPropertyA( sum, 7, VT_LPSTR, 0, NULL, ";1033" );
    MsiSummaryInfoSetPropertyA( sum, 9, VT_LPSTR, 0, NULL, "{6B0E4F2A-91C3-4D5E-8A7B-3C2D1E0F9A8B}" );
    MsiSummaryInfoSetPropertyA( sum, 14, VT_I4, 100, NULL, NULL );
    MsiSummaryInfoSetPropertyA( sum, 15, VT_I4, 0, NULL, NULL );
    MsiSummaryInfoPersist( sum );
    MsiCloseHandle( sum );
    for (i = 0; i < sizeof(sql) / sizeof(sql[0]); i++) if (run( db, sql[i] )) return 1;
    if ((r = MsiDatabaseCommit( db ))) { printf( "msidefault: commit %u\n", r ); return 1; }

    MsiSetInternalUI( INSTALLUILEVEL_FULL, NULL );
    sprintf( pkgname, "#%lu", (unsigned long)db );
    if ((r = MsiOpenPackageA( pkgname, &pkg ))) { printf( "msidefault: opening the package: %u\n", r ); return 1; }
    MsiCloseHandle( db );

    thread = CreateThread( NULL, 0, press_enter, NULL, 0, NULL );
    r = MsiDoActionA( pkg, "pick" );
    WaitForSingleObject( thread, INFINITE );
    GetExitCodeThread( thread, &code );
    MsiGetPropertyA( pkg, "CHOSEN", chosen, &size );
    MsiCloseHandle( pkg );
    DeleteFileA( path );
    printf( "msidefault: dialog %s, action %u, Enter chose \"%s\"\n", code ? "never showed" : "shown", r, chosen );
    if (code || strcmp( chosen, "next" )) return 1;
    printf( "msidefault ok\n" );
    return 0;
}
