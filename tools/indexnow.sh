#!/bin/sh
# Ping IndexNow after deploying a change worth reindexing.
#
# The key is public by design: it only proves the submitter controls the site,
# which is why the matching file has to stay reachable at the URL below.
# Reissue it by regenerating the .txt file and updating KEY here.
#
#   tools/indexnow.sh                   submits the homepage
#   tools/indexnow.sh /some/other/page  submits that path instead
#
# The key file it points at is site/<KEY>.txt, which is tracked alongside this
# script. It has to stay reachable at the site root or the submission is
# rejected, so it is deployed like any other page.

KEY=20116a2b48a4e637b95b39fe685d433a
SITE=https://printery.magikh0e.com
PATH_=${1:-/}

curl -s -o /dev/null -w "IndexNow: %{http_code}\n"   "https://api.indexnow.org/indexnow?url=$SITE$PATH_&key=$KEY&keyLocation=$SITE/$KEY.txt"
