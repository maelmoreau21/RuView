#!/bin/bash

# RuvSense Console static preview script.

echo "Starting RuvSense Console UI..."
echo ""
echo "Configuration:"
echo "   - UI Server: http://localhost:3000"
echo "   - Accueil: http://localhost:3000/ui/index.html"
echo "   - Console: http://localhost:3000/ui/console.html#fleet"
echo "   - Observatory: http://localhost:3000/ui/observatory.html"
echo "   - Pose Fusion: http://localhost:3000/ui/pose-fusion.html"
echo "   - Start ruvsense-master separately for live data"
echo ""

if lsof -Pi :3000 -sTCP:LISTEN -t >/dev/null ; then
    echo "Port 3000 is already in use. Stop the existing server or use another port."
    echo "You can manually start with: python -m http.server 3001"
    exit 1
fi

echo "Starting HTTP server on port 3000..."
echo "Press Ctrl+C to stop"
echo ""

cd "$(dirname "$0")/.." || exit 1
python -m http.server 3000
