/* A Unity Linux player's executable is only `main` calling UnityPlayer.so's
 * PlayerMain. Some (Unity 6's, e.g. Hollow Knight's) are linked at 0x200000,
 * inside gex's reserved low region; this is the same program built PIE.
 * muck-dotnet.sh UNITY_RELAUNCH=1 builds it under the game's executable name
 * in a directory of links to the game's files, since the player finds its
 * data from its own path (<name>_Data). */
int _Z10PlayerMainiPPc (int argc, char **argv);   /* PlayerMain(int, char**) */

int
main (int argc, char **argv)
{
	return _Z10PlayerMainiPPc (argc, argv);
}
