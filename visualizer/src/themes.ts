export interface ThemePreset { name: string; uiTheme: {primary: string; secondary: string; background: string}; graphTheme: {line1: string; line2: string} }

export const presets: ThemePreset[] = [
  {
    name: "Purple Dream",
    uiTheme: {
      primary: "#9b87f5",
      secondary: "#8E2DE2",
      background: "#090418"
    },
    graphTheme: {
      line1: "#4A00E0",
      line2: "#8E2DE2"
    }
  },
  {
    name: "Ocean Depths",
    uiTheme: {
      primary: "#36D1DC",
      secondary: "#5B86E5",
      background: "#0F2027"
    },
    graphTheme: {
      line1: "#36D1DC",
      line2: "#5B86E5"
    }
  },
  {
    name: "Sunset Vibes",
    uiTheme: {
      primary: "#FF8008",
      secondary: "#FFC837",
      background: "#1c0522"
    },
    graphTheme: {
      line1: "#FF8008",
      line2: "#FFC837"
    }
  },
  {
    name: "Neon City",
    uiTheme: {
      primary: "#f72585",
      secondary: "#7209b7",
      background: "#10002b"
    },
    graphTheme: {
      line1: "#4cc9f0",
      line2: "#f72585"
    }
  },
  {
    name: "Forest Mist",
    uiTheme: {
      primary: "#83EAF1",
      secondary: "#63A4FF",
      background: "#112211"
    },
    graphTheme: {
      line1: "#11998e",
      line2: "#38ef7d"
    }
  },
  {
    name: "Midnight Melancholy",
    uiTheme: {
      primary: "#a3a8e0",
      secondary: "#5c6bc0",
      background: "#0d1137"
    },
    graphTheme: {
      line1: "#7986cb",
      line2: "#3949ab"
    }
  },
  {
    name: "Euphoric Sunrise",
    uiTheme: {
      primary: "#ff9e80",
      secondary: "#ff6e40",
      background: "#1a0e23"
    },
    graphTheme: {
      line1: "#ffab91",
      line2: "#ff5722"
    }
  },
  {
    name: "Serenity",
    uiTheme: {
      primary: "#80deea",
      secondary: "#4dd0e1",
      background: "#051622"
    },
    graphTheme: {
      line1: "#26c6da",
      line2: "#00bcd4"
    }
  },
  {
    name: "Energetic Pulse",
    uiTheme: {
      primary: "#ffee58",
      secondary: "#ffca28",
      background: "#1a0024"
    },
    graphTheme: {
      line1: "#ffd54f",
      line2: "#ff8f00"
    }
  },
  {
    name: "Nostalgic Haze",
    uiTheme: {
      primary: "#ce93d8",
      secondary: "#ba68c8",
      background: "#120426"
    },
    graphTheme: {
      line1: "#ab47bc",
      line2: "#7b1fa2"
    }
  },
  {
    name: "Bittersweet",
    uiTheme: {
      primary: "#ef9a9a",
      secondary: "#e57373",
      background: "#21132e"
    },
    graphTheme: {
      line1: "#ef5350",
      line2: "#e53935"
    }
  },
  {
    name: "Cybernetic Trance",
    uiTheme: {
      primary: "#64ffda",
      secondary: "#1de9b6",
      background: "#091833"
    },
    graphTheme: {
      line1: "#00bfa5",
      line2: "#00897b"
    }
  }
];
